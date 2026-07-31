---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B3b fresh-context independent design reviewer
date: 2026-07-31
---

# B3b 独立设计审查 R1

## 结论

B3b 当前不能批准实施，verdict 为 `revise`。

§16 正确地把 Session Rollout 与 Project log分流，并继承 B3a 的 trusted codec、
fd-relative durability、checksum chain、exact reply、单writer typestate及checkpoint
规则。Question/Approval resolve与cancel所需的主要事实也进入了白名单。

但当前设计仍有五项会直接破坏重启恢复、cursor兼容或exact idempotency的重大问题：
随机stream ID与现有A4 wire语义矛盾；Turn prompt等继续执行所需状态不是事实；Runtime
重启对未终止Turn没有权威收敛协议；零事件command batch缺少sequence/framing定义且无
stream错误无法路由；全局ID分配和跨Session owner索引也没有可恢复来源。

## 重大问题

### M1 — 随机 `stream_id` 与 A4 将 `stream_id == SessionId` 的冻结协议矛盾

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1252-1257`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1283-1284`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1293-1294`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1322-1323`
  - `/home/mii/code/draft/alda-agent/src/protocol.rs:143-152`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:1077-1090`
- 实际证据：
  - §16要求每个Session有“固定随机`stream_id`”，并要求A1/A2 wire schema保持兼容、
    cursor truth table与A4一致。
  - 当前`SessionSnapshot`只返回`session_id`、epoch和covered sequence，没有独立
    `stream_id`字段。
  - 当前A4 `EventResume`明确执行
    `SessionId(cursor.stream_id.clone())`并据此查找Session；也就是说现有冻结语义是
    Session rollout的stream ID等于Session ID。
  - 若B3b实际使用随机stream ID，客户端无法从snapshot获得它，当前resume会把随机值当成
    Session ID并返回`SessionNotFound`；若仍使用Session ID，则违反§16稳定随机stream
    identity的磁盘设计。
- 影响：
  - 重启后的cursor无法可靠定位Session stream，§16最核心的resume验收不可实现。
  - 实施者任意选择一侧都会破坏另一侧：要么改变既有wire，要么让stored stream identity
    形同虚设。
- 最小修复方向：
  - 明确裁决一种兼容协议：MVP继续冻结`stream_id = canonical SessionId`；或版本化扩展
    `SessionSnapshot`显式返回随机rollout stream ID，并让resume通过独立stream registry
    映射到Session。
  - 固定start→snapshot→resume→restart的wire向量，验证客户端从正式响应取得的stream
    identity跨重启不变。

### M2 — `TurnStarted` 未保存原始prompt，pending Question重启后无法生成同一Approval

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1240-1248`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1265-1284`
  - `/home/mii/code/draft/alda-agent/src/protocol.rs:395-397`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:965-1004`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:1330-1341`
- 实际证据：
  - 当前`TurnStart`接收最多8000字符prompt，但`SessionEventKind::TurnStarted`只记录
    `turn_id`；prompt仅存在进程内`ServiceState.turn_prompts`。
  - Question被回答时，App Service从该内存map取回prompt，并把它纳入
    `approval_subject_digest`。该digest是后续Approval响应的权限绑定字段。
  - §16声称重启在Question Pending切点可恢复并继续相同命令路径，却没有任何stored
    event/checkpoint whitelist字段保存prompt或等价canonical approval subject inputs。
  - checkpoint是事实投影缓存，不能凭空补造日志未包含的数据。
- 影响：
  - 重启后的Question虽然能显示和回答，却无法确定性构造原Approval subject digest；
    猜测/空prompt会改变授权对象，复用内存外数据则让projection不再可从事实重建。
  - 这不是UI附加字段，而是ModelEgress审批能力的安全绑定内容。
- 最小修复方向：
  - 在versioned stored `TurnStarted`或独立authoritative context fact中冻结canonical
    prompt/subject inputs及大小上限；reducer projection保存继续Question→Approval所需的
    最小白名单。
  - 加入Question Pending处重启、回答后approval digest与无重启路径逐字节相同的向量。

### M3 — Runtime重启时未终止Turn的权威状态转换没有定义

- 位置：
  - `/home/mii/code/draft/docs/design/mvp-design.md:190-206`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1265-1282`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1316-1319`
  - `/home/mii/code/draft/alda-agent/src/protocol.rs:339-363`
- 实际证据：
  - MVP要求Runtime重启时仍在运行的Provider/Alda/播放任务记录
    `AbortedByRestart`，不自动重放副作用；pending Question/Approval则保留并重投，若
    owner Turn已终止要形成`OwnerTurnAborted`。
  - `TurnStatus`还包含`BudgetExceeded`与`AbortedByRestart`，但§16 reducer规则只冻结
    `Succeeded/Failed/Cancelled`的前置路径。
  - §16没有定义startup reconciliation：哪些`Running`/`CancelRequested`/
    `WaitingForInput` Turn保持可继续，哪些必须追加`TurnCompleted(AbortedByRestart)`，
    owner-abort事件与terminal事件的顺序、这些恢复事实由哪个幂等transaction ID提交。
  - 验收只检查Question Pending、Approval Pending和terminal三个切点，没有“运行中且无
    pending对象”“cancel requested后崩溃”或restart reconciliation再次崩溃。
- 影响：
  - 重启后可能把已经消失的Runtime任务继续投影为Running，或错误终止本应持久等待的
    Question/Approval owner Turn；两者均违反权威生命周期。
  - reconciliation若非幂等事实事务，连续重启会重复terminal/owner-abort事实。
- 最小修复方向：
  - 冻结完整Turn状态转移表，覆盖`BudgetExceeded`和`AbortedByRestart`；定义启动时按
    durable事实判断runtime-owned与Session-owned等待状态。
  - 定义可重入的restart reconciliation batch及稳定restart/dispatch identity，明确
    owner-abort→TurnCompleted顺序；增加每个非终态切点和reconciliation中途再崩溃测试。

### M4 — 零事件command-only batch没有sequence语义，且无有效Session的拒绝无法归档

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1259-1261`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1288-1297`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1324-1325`
  - `/home/mii/code/draft/docs/design/mvp-design.md:180-182`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:1144-1165`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:1265-1304`
- 实际证据：
  - batch仍要求`first_sequence/last_sequence`，Session event sequence从1连续递增；
    §16允许`events=[]`，却没有定义空批的first/last值、是否推进head、连续性公式，以及
    多个零事件批次夹在事件批次间如何验证。
  - checkpoint同时锚定covered sequence/checksum/offset；零事件batch会造成多个不同
    checksum共享同一event head，若规则不冻结，full replay与checkpoint tail可能选择
    不同anchor语义。
  - “拒绝且无事实的稳定业务回复”不一定有可写Session：`SessionNotFound`没有stream；
    Turn/Question/Approval ownership mismatch中，请求携带Session B而对象实际属于
    Session A。设计没有规定reply写入哪一流，也没有规定哪些validation错误在定位stream
    前不承诺durable idempotency。
  - `SessionStart`自身在成功前也没有既有Session command index可容纳其stable reply。
- 影响：
  - 空批实现可能破坏sequence/checksum replay，或者让cursor head因无事件命令变化。
  - 更严重的是“所有改变权威状态命令幂等结果持久化”的边界不闭合：SessionStart和无效/
    错owner请求在per-Session log模型中没有唯一归档位置。
- 最小修复方向：
  - 冻结command-only framing：例如first/last均等于当前event head且明确不推进event
    sequence，但必须推进batch chain/offset；scan、checkpoint和cursor分别按batch/event
    维度验证。
  - 明确幂等路由矩阵：哪些命令在解析出受信Session后进入该Session log；SessionStart及
    无stream错误由何种独立client/control stream持久化，或明确不属于B3b durable承诺。
  - 对连续空批、空批夹事件、checkpoint跨空批、SessionStart response-before-crash及
    wrong-owner重试加入固定向量。

### M5 — 进程级ID计数器与跨Session owner索引没有事实化重建规则

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1271-1279`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1316-1321`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:535-555`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:923-938`
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:965-1004`
- 实际证据：
  - 当前Session/Turn/Question/Approval ID来自进程内全局递增计数器；重启后全部归零。
  - ownership错误依赖全局`turn_owners`、`question_owners`、`approval_owners` map，而
    §16 projection只承诺单Session ownership/objects，没有定义启动时如何枚举所有
    fd-relative Session目录、验证目录hash与stored ID、构建全局唯一索引。
  - 若重启后从1重新分配，新的Session或现有Session中的新Turn会与历史ID碰撞；单Session
    reducer能拒绝本stream重复，却不能判断同一Turn ID已属于另一个Session。
  - 仅取“本Session最大数字+1”也不保留当前全局ID命名语义，并无法为新Session分配唯一
    `session-N`。
- 影响：
  - 重启后的首个创建命令可能稳定失败、错误命中旧owner，或把跨Session请求从
    `OwnershipMismatch`降级成`NotFound`，破坏A2冻结回复与exact idempotency。
  - filesystem中存在但未安全枚举/索引的Session也可能被同名重新创建，造成目录复用和
    stream identity冲突。
- 最小修复方向：
  - 改用具有碰撞检查的随机/单调持久ID，或增加durable allocator/control stream；冻结
    crash后分配规则，不能依赖内存counter。
  - 定义descriptor-relative sessions enumeration与启动索引构建：逐目录验证canonical
    hash、stored Session ID、stream ID，检测跨Session Turn/Question/Approval重复并
    fail closed。
  - 增加多Session重启后继续创建、历史最大ID、重复owner ID、目录hash错配及新Session
    创建崩溃重试测试。

## 已确认的非阻断设计性质

- Session与Project使用独立registry、文件、stream、epoch及sequence，不复用万能event
  enum。
- stored codec要求primitive-only、逐字段构造，避免直接Deserialize live protocol/
  capability；这是正确边界，实施时需延续B3a最终修复。
- Question/Approval requested、resolved、owner-aborted及Turn cancel/completed进入同一
  事务候选，取消时按created sequence稳定排序。
- exact raw reply、client+command key、payload digest冲突、poisoned writer、
  compare-and-truncate和checkpoint prefix anchor均复用已RELEASE的B3a协议。
- cursor不写command index、retention/compaction明确排除、epoch固定1；不会用epoch变化
  掩盖日志损坏。
- fd-relative、current-owner/private、regular/nonblocking、有界读取及file→parent-dir
  同步规则已明确继承B3a。

## 审查判定

修订M1–M5后，B3b才可进入实施。所有问题都属于Session持久化协议本身，无需提前接入B4
生产App Service，因此 verdict为`revise`，不是`blocked`。
