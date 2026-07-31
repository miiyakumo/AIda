---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B3a fresh-context independent design reviewer
date: 2026-07-31
---

# B3a 独立设计审查

## 结论

B3a 当前不能批准实施，verdict 为 `revise`。

§15 已正确选择独立 Project stream、单行事务批次、checksum chain、newline 尾部判定、
descriptor-relative Linux 存储、同一 B1 reducer replay，以及“事实日志权威、
checkpoint 可丢弃”的总体方向。它也没有提前接入生产 App Service，符合 B4 的范围边界。

但当前设计仍有五项会破坏崩溃后精确幂等、checkpoint 等价性或写者安全边界的重大问题：
stable reply 的字节表示未冻结；checkpoint 没有保存 prefix command index；append 的
不确定 I/O 结果没有定义 writer 隔离/恢复；无密钥 checksum 无法证明“重算 checksum
后的伪造授权事实必被拒绝”；实例锁 token 也没有实际建立“每 Project 恰好一个 writer”
的租约。

## 重大问题

### M1 — `stable_reply_json` 没有冻结可同时满足 exact bytes 与 canonical checksum 的表示

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1087-1097`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1130-1132`
  - `/home/mii/code/draft/docs/design/mvp-design.md:277-280`
- 实际证据：
  - batch 字段只写成 `stable_reply_json`，没有说明它是结构化 JSON value、原始 UTF-8
    bytes、canonical DTO bytes，还是 JSON 字符串中的转义内容。
  - 同一段又禁止 map/float 进入 checksum canonical bytes，但通常的 reply JSON 是 object，
    会包含 map；若 scan 时 parse 为 value 后再 serialize，空白、对象 key 顺序和数值表示
    都可能改变，无法保证返回提交时的 exact bytes。
  - 设计要求 64 KiB 上限，却未定义按原始 reply bytes、转义后的 batch bytes还是解析后
    value 计数，也没有固定 reply byte vector。
- 影响：
  - fsync 后响应前崩溃的核心保证是重试得到批次内“原结果”；表示未冻结会让恢复路径返回
    语义相同但字节不同的响应，或让 checksum 编码依赖 serde/map 顺序。
  - 实施者无法写出唯一兼容的 v1 codec，后续澄清会改变已经落盘的 batch checksum。
- 最小修复方向：
  - 将稳定响应定义为明确版本化 response DTO 的唯一 canonical UTF-8 bytes，或以
    base64/byte-array 无损封装原始 bytes；checksum 对封装后的固定 tuple 计算。
  - 冻结长度计数点、UTF-8/非 UTF-8 策略、JSON key 顺序/数值规则，并加入原始 reply
    bytes、batch bytes、recovered reply bytes 三者逐字节相等的固定向量。

### M2 — checkpoint 未包含 command index，无法实现声明的 checkpoint + tail 幂等等价

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1124-1132`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1148-1156`
- 实际证据：
  - checkpoint 字段清单只有 projection schema、stream/epoch、covered sequence/checksum、
    projection digest、`ProjectSnapshot` 和 checkpoint checksum。
  - 已提交 command ID、client ID、payload digest、reply protocol version 和 exact stable
    reply 都只存在于被 checkpoint 覆盖的旧 batch 中；B1 `ProjectSnapshot` 不是 command
    index。
  - §15 同时要求 checkpoint + tail replay 与 full replay 的 `command index` 相同，却没有
    要求 load 时扫描 covered prefix，也没有把 prefix command records 放进 checkpoint。
- 影响：
  - 真正从 checkpoint offset 只 replay tail 时，重启后重投一个早于 checkpoint 的命令
    会被当成新命令并追加重复 Revision，或无法返回原 reply，直接违反 MVP §9.2。
  - 若实现偷偷从头扫描 command records，则 checkpoint 协议和验证成本与设计不符，
    也没有说明如何把扫描得到的 index与 checkpoint covered checksum绑定。
- 最小修复方向：
  - 明确二选一：checkpoint 保存并校验截至 covered batch 的完整 durable command index
    （包含 exact reply bytes）；或 load 始终验证/扫描 log prefix 来重建 command index。
  - 对 checkpoint 前命令的同 digest 重投、不同 digest 冲突及 byte-exact reply加入强制
    恢复测试。

### M3 — write/sync 返回错误后的文件状态不确定，但设计允许 writer state继续保持旧值

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1122-1128`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1134-1144`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1167-1178`
- 实际证据：
  - projection/command index只在 `sync_all` 成功后更新；但 `write_all`、`flush` 或
    `sync_all` 返回错误时，文件可能已有 partial line，也可能已有完整 newline batch。
  - 当前恢复规则只描述 reopen/scan/repair，没有规定发生任一不确定 append I/O error 后
    当前 `ProjectLogWriter` 必须被 poison、关闭并重新 scan，或如何协调一个已完整落盘但
    sync 返回错误的 command。
  - 下一次 append若继续使用旧 sequence/checksum/index，可能把新行追加在 incomplete tail
    后，或重复已经完整存在的 transaction/command。
- 影响：
  - 一次可恢复 I/O error 会把中间尾损坏变成后续完整记录之前的永久 corruption，或产生
    重复 command；这突破“损坏尾只影响未完成提交”和 exact idempotency。
- 最小修复方向：
  - 将首次可能改变 events fd 的错误定义为 writer poisoned：禁止任何后续 append/响应，
    必须关闭并在同一排他 lease下重新 scan；incomplete tail走 compare-and-truncate，
    完整合法 batch则按已提交事实恢复 command/reply。
  - failpoint矩阵增加“不 reopen 直接第二次 append必须拒绝”，以及 sync 返回错误但完整
    newline可见时重扫后同 command不重复的测试。

### M4 — unkeyed checksum 只能检测损坏，不能拒绝字段合法且 checksum 已重算的授权伪造

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1093-1102`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1104-1118`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1184-1185`
- 实际证据：
  - batch chain 使用普通 SHA-256，没有 secret、MAC、signature 或外部不可伪造锚点。
  - trusted replay 的目的正是从 stored DTO 重建过去的 Human/Artifact 审计事实，而不持有
    live `HumanActor`/Store receipt capability。构造器只能检查 ID、字段关系、commit
    identity格式与 reducer不变量。
  - 因而能读取日志和 manifest 的主体可以复制一个字段合法的 human decision，或按公开
    canonical规则构造一致的 Artifact audit fact，再重算该批及后续 checksum；它与合法
    历史在 codec输入上不可区分。当前验收却要求这种伪造必定失败。
- 影响：
  - 这是不可实现的验收条件，容易诱使实现把 stored DTO直接升级为 live capability，
    恰好重新打开 B1 已关闭的授权绕过；反之正常 trusted replay会通过该 fixture而测试失败。
- 最小修复方向：
  - 明确威胁模型：若 0700/current-uid root 是信任边界，checksum只承诺 accidental
    corruption/torn-write检测，删除“合法字段 + 重算全链仍拒绝”的声明，测试仅验证
    stored serde不能进入 live mutation API。
  - 若确实要防同 UID 离线改写，必须增加密钥化 MAC/signature及安全密钥生命周期/轮换，
    并说明 B2 commit identity如何被不可伪造地绑定；普通 SHA-256 chain不够。

### M5 — 实例锁 capability没有定义每 Project writer 的唯一、不可复制租约

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1074-1076`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1122-1127`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1186`
- 实际证据：
  - 设计声称每个 Project 同时最多一个 writer，并要求“两 writer拒绝”，但唯一机制只写
    “构造要求 B4 实例锁 token”。
  - 一个全局实例锁证明没有第二进程，并不自动阻止同一进程用相同 token为同一 Project
    构造两个 writer；设计没有规定 token不可 Clone/必须被 writer独占借用，也没有
    Project writer registry、per-project lease或第二个 advisory fd lock。
  - 两个 writer各自持有 events fd仍可基于同一旧 sequence/checksum dry-run并交错写入；
    单次 `write_all`不是 transaction-level mutual exclusion证明。
- 影响：
  - 重复 writer会产生相同 sequence、分叉 checksum chain、重复 command或字节交错，
    破坏 B3a 全部 framing/replay保证。sealed constructor只限制调用者身份，不限制实例数。
- 最小修复方向：
  - 冻结具体租约协议：实例锁 owner维护按 Project key唯一的不可复制 writer lease，
    writer Drop释放；或在 Project fd上持有并验证独占 OS lock。构造必须原子拒绝第二 writer，
    repair与 append共享同一 lease。
  - 测试同 token同 Project第二 writer、不同线程竞态、Drop后重开、不同 Project并行和
    repair/append互斥，而不只测试“无 token不能写”。

## 已验证的非阻断设计性质

- Project 与 Session stream、sequence、epoch明确分离；B3a没有把两种领域事实塞进同一
  event enum。
- newline framing对 incomplete final fragment与完整 newline corruption作了清楚区分；
  中间坏行不跳过，repair要求重新 scan、长度/digest compare、truncate及 file/dir sync。
- root从 `/` fd逐组件 no-follow解析，受管对象 descriptor-relative、current UID、
  private mode和目录逐级 fsync，沿用了已 RELEASE 的 B2安全边界。
- batch非空、sequence连续、chain checksum、stream/project匹配和 reducer replay共同
  fail closed；Project ID使用canonical hash目录且批内复核真实 ID。
- B1 domain event与授权事实仍不公开 Deserialize；stored DTO与 live mutation surface
  分离是正确方向。
- checkpoint被定义为可删除派生缓存，损坏时 full replay，不会反向覆盖权威日志。
- failpoint清单已覆盖初始化、append、checkpoint和repair的主要 barrier；闭合 M3/M5 后
  还需把 writer poison/lease race纳入矩阵。

## 审查判定

修订 M1–M5 后，B3a 才可进入实施。当前问题均可在现有 B3a 范围内解决，不需要提前接入
B4生产 App Service；因此 verdict 为 `revise`，不是 `blocked`。
