---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B4 fresh-context independent design reviewer
date: 2026-07-31
---

# B4 独立设计审查 R1

## 结论

B4 当前不能批准实施，verdict 为 `revise`。

§17选择control WAL协调Project/Session两个不可原子rename的日志，方向符合MVP
§9.2；单实例锁、恢复后再bind、published read view、固定Project→Session顺序、
startup reconciliation、有界blocking和完整故障矩阵也覆盖了正确风险面。

但当前协议仍有六项会让Prepared无法恢复、create重复分配或运行中暴露不一致状态的重大
问题：

1. Prepared只冻结event digest/identity，没有可重建完整aggregate events的primitive
   payload；
2. opaque Artifact receipt在Prepared后崩溃时无法重新产生，却没有安全的durable-plan
   conversion入口；
3. allocation catalog fact与Prepared/reply没有冻结为同一个control原子batch；
4. B3当前不支持“transaction ID + digest”的幂等probe，control recovery无法区分已完成
   aggregate与冲突；
5. 协议只定义startup补偿，没有定义live半提交后actor必须进入隔离恢复状态；
6. 不可Clone的同一instance-lock capability如何同时构造/约束B2与B3尚未形成可实现的
   ownership模型。

## 重大问题

### M1 — Prepared只有event digest，崩溃后无法重建完整B3 append request

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1422-1430`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1437-1444`
- 实际证据：
  - `CommandPreparedV1`字段只概括为`project_plan?`/`session_plan?`；紧接着将plan定义为
    “transaction identity + expected pre-head + event digest”。
  - recovery却要求“从已验证plan构造B3 append request”。transaction ID、head和digest
    只能验证一个已有event vector，不能从digest反推出Project或Session event payload。
  - 例如TurnStart需要Turn ID、canonical prompt、Question ID/prompt/choices；Approval
    path需要Artifact audit、Approval decision/responder及terminal事实。这些值在Prepared
    fsync后、aggregate append前崩溃时只存在于丢失的内存plan。
- 影响：
  - WAL在最关键的Prepared-only状态下只能证明“曾计划某些events”，却不能redo它们；
    恢复要么永久fail closed，要么从当前投影/命令重新规划并可能生成不同ID、reply或
    approval subject。
  - 这违反write-ahead redo log的基本要求和§17“Prepared未完成启动补齐”承诺。
- 最小修复方向：
  - 冻结versioned、primitive-only `StoredProjectPlanV1` /
    `StoredSessionPlanV1`，包含完整canonical event DTO、expected pre-state、transaction
    ID、event bytes/digest及必要reply关联。
  - control codec逐字段验证plan，再通过B3专用trusted-plan conversion生成append
    request；固定Prepared bytes→recovered append bytes向量，证明不依赖内存命令重跑。

### M2 — Artifact receipt是不可重建的live capability，Prepared恢复边界没有闭合

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1423-1430`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1434-1444`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1477-1482`
  - `/home/mii/code/draft/alda-agent/src/artifact_store.rs:69-80`
  - `/home/mii/code/draft/alda-agent/src/artifact_store.rs:130-138`
  - `/home/mii/code/draft/alda-agent/src/state/mod.rs:560-566`
- 实际证据：
  - `CommittedArtifactReceipt`有私有字段、不实现Clone/Deserialize，并由
    `register_artifact`消费后才能产生live `ArtifactRegistered`。
  - §17却把`artifact_receipts[]`写入可序列化control Prepared，并称恢复时重新验证
    receipt/audit identity；真正的opaque receipt既不能序列化，也无法仅凭已存在blob重新
    mint。
  - 若在B2 put与Prepared fsync后崩溃，原receipt随进程消失；恢复即使same-handle verify
    blob，也没有设计批准的API把“已验证blob + Prepared audit plan”安全转换成Project
    stored fact。
  - 直接从Prepared字段构造`ArtifactRegistered`会绕过B2 receipt唯一live producer；
    强行给receipt增加Deserialize又会撤销B1/B2已RELEASE的capability边界。
- 影响：
  - Approval跨aggregate矩阵的Prepared→Project append路径不可实现，或会引入一个能伪造
    VerifiedDurable reachability的通用反序列化入口。
- 最小修复方向：
  - Prepared保存独立primitive artifact audit plan，不保存/声称保存opaque receipt。
  - 冻结一个窄的recovery-only consuming API：绑定State instance/control tx与B2 Store
    instance，对同一handle重新verify hash/size，重算commit identity后产生一次性
    recovery capability；只能交给对应Prepared plan conversion，不能进入普通live
    mutation/wire。
  - 测试receipt丢失后的redo、错误Store instance、替换blob、plan audit篡改和重复消费。

### M3 — create allocation与Prepared exact reply没有原子绑定

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1422-1426`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1432-1450`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1473-1476`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1505-1506`
- 实际证据：
  - control事实白名单将`ProjectAllocated`/`SessionAllocated`与`CommandPreparedV1`列为
    独立records，但协议顺序只说明“生成ID和reply→append Prepared”，没有说明allocation
    fact与Prepared是否在同一checksum batch/fsync。
  - 若先fsync Allocation再单独fsync Prepared，二者之间崩溃后global command index没有
    command→allocated ID映射；同command重试可能分配第二ID。
  - 若先Prepared后Allocation，则Prepared plan可引用catalog尚不存在的Project/Session，
    recovery双向目录检查与aggregate创建顺序没有定义。
  - 单靠最终`CommandCommitted`不能修复，因为create的exact reply和ID必须在第一次
    authoritative fsync时就稳定。
- 影响：
  - ProjectCreate/SessionStart在control切点崩溃后可能产生重复目录、orphan allocation或
    返回不同ID，直接违反create exact idempotency验收。
- 最小修复方向：
  - 冻结control事务batch语义：create的Allocation + Prepared command record +
    exact reply + full aggregate plan必须同一行、同一checksum、同一次fsync成为原子
    control事实。
  - 定义Session control catalog已提交而首Session batch未提交的redo状态，以及Project
    对称状态；禁止独立allocation record先行可见。

### M4 — aggregate transaction重复检查缺少可验证digest，redo协议与现有B3语义不兼容

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1428-1444`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1024-1025`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1067-1068`
- 实际证据：
  - §17要求aggregate中“同transaction ID且digest相同视为完成，不同则fail closed”。
  - 当前B3a replay只保存`HashSet<String>` transaction IDs；重复ID直接返回sequence错误，
    没有保存或公开该transaction对应batch/event digest。
  - B3b同样以transaction ID集合做唯一性，checkpoint也只保存ID列表。
  - 因此control恢复看到Project head已前进时，无法用公开trusted API证明目标global tx
    已以**同一plan**提交；再次append会被拒绝，单凭head/last checksum也不能定位任意
    历史tx。
- 影响：
  - 崩溃发生在aggregate fsync后、control commit前是正常核心路径；当前设计依赖一个B3
    尚不存在、数据模型也未支持的幂等判断。
  - 若把任何重复transaction ID都当成功，篡改/编程错误可把不同events冒充已完成。
- 最小修复方向：
  - B3 Project/Session transaction index保存
    `transaction_id -> canonical batch/event digest + resulting last sequence/checksum`，
    并进入checkpoint/full replay等价性。
  - 提供只读`probe_transaction`或append-idempotent结果，严格区分Absent /
    SamePlanCommitted / ConflictingPlan；control redo只能在SamePlan时跳过。

### M5 — live半提交没有隔离/恢复状态，startup-only补偿不足

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1432-1444`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1454-1462`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1483-1498`
- 实际证据：
  - 协议定义Project→Session append后写Committed，并规定**启动恢复**补齐Prepared；
    没有定义当前进程中Project append成功、Session append或control commit失败时actor
    进入何种状态。
  - B3 writer的内存projection已在各自append成功后前进，而published read view承诺只在
    Committed后更新；此时继续处理下一命令会同时面对旧published view、新writer head和
    pending control transaction。
  - “in-memory projection前”failpoint与“response前”测试只说明崩溃重开结果，没有要求
    不崩溃的I/O error后停止queries/commands、原lease下redo，或关闭服务触发干净重启。
- 影响：
  - 一个普通可恢复I/O error可让进程继续服务旧snapshot、接受基于旧head的命令，或在
    pending tx之上追加后续aggregate事实，破坏control顺序与对外全有/全无。
  - 仅保证下次进程启动修复，不保证当前进程在错误到退出之间不暴露半提交。
- 最小修复方向：
  - durable actor使用显式`Ready | Recovering(global_tx) | Fatal` typestate。Prepared
    fsync后任何aggregate/control error都必须停止命令、query和broadcast发布；在同一
    writer leases下完成同一redo并Committed，或使服务fatal并停止监听。
  - 所有query从单一committed read view读取，Recovering期间返回typed unavailable，
    不能回退内存backend；加入不终止进程的每个I/O error与并发query/command测试。

### M6 — 一个不可Clone lock capability无法按当前文字同时被B2与B3拥有

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1406-1414`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:300-327`
- 实际证据：
  - §17要求实例锁能力不可Clone，并称它“同时构造B2/B3 store”。
  - 当前`StateStore::open`按值消费并长期保存`StateStoreInstanceLease`；同一个线性值被
    move后不能再传给Artifact Store/control Store。
  - B2当前没有实例lease参数，因此“锁fd关闭后整个durable App Service不可继续写”也没有
    由类型/ownership连接到B2 API。
- 影响：
  - 实施者只能Clone/伪造多个token、让某些Store不受lock lifetime约束，或大改构造方式；
    三种选择的安全语义不同，设计没有可验证裁决。
- 最小修复方向：
  - 让顶层`DurableRuntime`唯一拥有lock fd，并在其生命周期内构造/拥有B2、B3、control；
    stores接收不可自行构造的借用/weak guard或由runtime私有调用，不各自占有同一线性值。
  - 明确shutdown/drop顺序及lock health语义；测试不能脱离live顶层guard取得生产writer，
    第二实例在任何Store打开前失败。

## 已确认的非阻断设计性质

- 显式absolute private data root、从`/`逐组件no-follow打开、nonblocking advisory
  instance lock及“恢复前不bind”是正确启动边界。
- control WAL作为协调事实源而非尝试跨文件rename，固定Project→Session顺序合理。
- Prepared前Artifact仅为orphan、Committed后才发布read view/broadcast，符合MVP可见性
  原则。
- catalog与Project/Session目录双向核对、全局owner重复fail closed、unknown目录不跳过，
  延续B3安全模型。
- Pending input保留、Running/CancelRequested通过B3b planner提交restart事实，且恢复
  完成前不接流量。
- Artifact下载同时要求Project reachability与B2 same-handle verify，不把blob存在冒充
  授权。
- 内存backend只保留显式测试、生产serve不得fallback；磁盘工作进入专用writer/
  blocking边界而不阻塞Tokio worker。
- failpoint和真实drop/reopen验收覆盖lock、control、aggregate、projection、response、
  restart及HTTP/WS/CLI端到端，方向充分；修订M1–M6后才能形成可执行断言。

## 审查判定

修订M1–M6后，B4才可进入实施。问题均属于当前production integration协议，不需要引入
真实Provider/Alda或C阶段Permission Broker，因此 verdict为`revise`，不是`blocked`。
