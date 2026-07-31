---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B3a second-round fresh-context independent design reviewer
date: 2026-07-31
---

# B3a 第二轮独立设计审查

## 结论

§15 已逐项吸收 R1 的 M1–M5：

- stable reply 固定为 canonical UTF-8 bytes，以 standard base64 无损保存并返回原 bytes；
- checkpoint 保存截至 covered batch 的完整 durable command index；
- write/flush/sync 不确定结果会永久 poison 当前 writer；
- checksum 威胁模型诚实限定为 accidental corruption/torn write；
- `StateStore` 以 mutex registry 原子签发每 Project 唯一、不可 Clone 的 writer lease。

这些修订本身方向正确，并与 B1 trusted stored-event 边界、B2 descriptor/durability
模型及 MVP exact idempotency 一致。

但 verdict 仍为 `revise`。M3 与 M5 组合后产生一个新的恢复协议空洞：poisoned writer
永久禁止 repair，而不可 Clone 的 Project lease 又由该 writer 独占；设计要求“在同一
lease 下关闭 fd 并重新 scan”，却没有冻结能够原子转移 lease 的 consuming recovery
操作。若 Drop poisoned writer 再调用 `open_project_writer`，registry key 会短暂释放，
其他线程可抢占；若不 Drop，第二 writer又必须被 registry拒绝。核心 append-error恢复
路径因而没有唯一可实现语义。

只需补充 `recover_poisoned(self)`/等价 typestate transition：消费 poisoned writer，
保留并转移同一个 lease，在不释放 registry key的情况下关闭旧 fd、重扫并返回
Recovered/CleanupRequired/Corrupt 结果。无需扩大到 B4 production integration。

## R1 M1–M5 闭合核对

| R1 项 | 判定 | 第二版证据 |
|---|---|---|
| M1 canonical raw reply | 已闭合 | §15:1090-1107 冻结 command record、raw_len、standard base64、canonical UTF-8 DTO bytes、parse/re-encode byte equality、64 KiB raw 上限及五项固定向量；恢复返回 decoded 原 bytes。 |
| M2 checkpoint command index | 已闭合 | §15:1164-1174 保存完整 prefix command index、稳定排序、逐项 canonical reply复验，并要求 full replay 与 checkpoint+tail 的 projection/index/reply bytes 全等；§15:1210 覆盖 checkpoint 前重投。 |
| M3 poisoned writer recovery | **部分闭合，仍阻断** | §15:1140-1144 已正确 poison 并禁止旧状态继续写；但与 nonclone lease/registry 的恢复所有权转移未定义，详见 M1。 |
| M4 checksum threat model | 已闭合 | §15:1204-1207 明确 0700/current-uid root 是信任边界，SHA-256 只检测意外损坏/torn write；字段完全合法且重算全链的同 UID 改写不在承诺内，未来抗篡改需另设计 MAC/key lifecycle。 |
| M5 per-Project unique lease | **在线签发已闭合；poison recovery交互仍阻断** | §15:1074-1079 定义 mutex `HashSet<ProjectKey>`、原子 check+insert、nonclone lease、Drop释放、同 Project竞态拒绝、不同 Project并行及 repair/append互斥；但未定义保持 registry占用的恢复转移。 |

## 必须修复项

### M1 — poisoned writer 无法在不释放唯一 lease 的情况下重新 scan/recover

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1074-1079`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1140-1144`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1158-1160`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1195-1201`
- 实际证据：
  - registry 要求相同 Project 同时只有一个不可 Clone 的 `ProjectWriterLease`，writer
    Drop 才释放 key。
  - 发生任何可能改变 fd 的错误后，“该实例禁止继续 append、repair”，但又要求“在同一
    Project lease 下关闭 fd 并重新 scan”。
  - 设计没有定义谁拥有 lease、poisoned writer 是否可被消费、旧 fd 如何先关闭、scan
    结果如何携带原 lease构造新 writer，也没有说明 recovery失败时 registry key何时释放。
  - 如果先 Drop poisoned writer，key 被释放，另一个线程可能在 scan/repair前取得新
    writer；这违反“同一 lease”与 repair/append互斥。
  - 如果保持 poisoned writer存活，再调用普通 `open_project_writer`，registry 应按 M5
    正确拒绝第二 writer，恢复无法开始。
- 影响：
  - partial write 或 sync-unknown 后，系统要么永久无法恢复该 Project writer，要么必须
    临时释放排他性并允许另一 append与 rescan/repair竞态。
  - 这是 MVP 崩溃一致性路径的核心状态转换；实现者若自行选择不同方案，会让
    `WriterPoisoned`、repair token与唯一 writer测试产生不兼容 API/语义。
- 最小修复方向：
  - 冻结 consuming typestate API，例如
    `PoisonedProjectWriter::recover(self) -> RecoveryOutcome`。它移动而非 Clone
    `ProjectWriterLease`，关闭旧 events fd，以只读新 fd从 offset 0重扫；全过程
    registry key保持占用。
  - 恢复结果至少区分：
    - 完整合法 batch已经存在：返回持同一 lease 的 Ready writer及恢复的 reply/index；
    - incomplete tail：返回持同一 lease 的 RepairRequired handle，只有它可执行
      compare-and-truncate；
    - committed-area corruption：返回 Corrupt并释放/封存 lease，绝不提供 append。
  - 定义 panic/error/Drop时的 key释放，防止 registry永久泄漏。
  - 增加测试：poison后普通第二 open仍拒绝；consuming recovery期间竞态 open拒绝；
    recovered writer连续 sequence；repair前后同一 lease；recovery Drop后才可重开。

## 已确认闭合的关键契约

### Exact stable reply

- reply在提交前必须是 versioned `CommandReply` 的唯一 canonical compact JSON；
  protocol/client command identity会复核。
- raw length在 base64前计数，stored representation固定 RFC 4648 standard alphabet、
  padding且无换行。
- scan验证 base64、raw_len、UTF-8、DTO与 canonical re-encode，但 index保存并返回原始
  decoded bytes，避免 parse-reserialize 漂移。
- command index key为 client+command ID；同 digest不 append并逐字节返回旧 reply，
  不同 digest返回 conflict。

### Checkpoint

- checkpoint包含完整 prefix command index，而不仅是 B1 projection。
- covered checksum必须真实存在于 log prefix；Project/stream/epoch/projection digest/
  reducer schema均须匹配。
- checkpoint损坏或旧 schema只丢缓存，full replay仍是权威路径。

### Honest integrity model

- checksum tuple和chain足以检测 line损坏、重排、删中间行、跨流拼接与 torn tail。
- stored DTO仍需经过 field/audit/reducer验证；字段无效即使重算 checksum也失败。
- 字段完全合法、可重算全链的同 UID离线改写明确不在 SHA-256 保证内，没有把普通
  checksum误称为授权证明。

### Writer registry

- `StateStore` 的全局 instance lease不可复制；
- per-Project key在 mutex临界区原子 check+insert，消除同进程 check-then-act；
- writer lease不可 Clone，Drop释放；reader可并存，不同 Project可并行；
- repair与append声明共用同一 lease。缺口只在 poison→recovery 的所有权转移。

## 与 B1/B2/MVP 的对照

- B1 `ProjectEvent`、Human/Waiver/Artifact fact继续不公开 Deserialize；B3a使用独立
  stored DTO并在可信 scan后转换，没有重新把 stored serde开放给 live mutation。
- B2 receipt仍是 live `ArtifactRegistered` 唯一 producer；replay只重建已提交 audit
  fact并重算 commit identity，不伪造 opaque receipt。
- Project与Session日志分流、各自 sequence/epoch，不把 Rollout事件混入作品事实。
- 单批 fsync是唯一提交点，符合 MVP §9.2；checkpoint失败不回滚权威 log。
- incomplete final fragment与已有 newline的 corrupt batch严格区分；中间坏行不跳过。
- 当前设计没有提前接 App Service写面或声称B4重启恢复完成。

## 非阻断项

- checkpoint generation使用 no-replace文件并另有 latest pointer，但 pointer的具体
  atomic replace协议尚未完全展开。只要 load始终验证 generation内容及 log covered
  checksum、pointer错误回退 full replay，它只是缓存发现效率问题；实施时应复用
  temp→sync→atomic replace→dir sync，不得用覆盖写。
- command index随历史增长可能使checkpoint变大；B3a未冻结checkpoint上限。它不影响
  权威 full replay正确性，但实现应有checked size/entry count，超限时跳过checkpoint而
  不是OOM或截断幂等记录。
- `stable_reply_protocol_version` 与外层 batch schema已经分离是正确的；未来协议升级时
  必须保留旧 reply bytes的返回/迁移策略，不能仅用当前 serializer重编码旧版本。
- checksum不抵抗同 UID攻击是已接受威胁模型，不应在实施审查中重新提升为缺陷。

## 审查判定

M1、M2、M4 已闭合；M3/M5 各自的主要修订正确，但 poison recovery与nonclone lease之间
缺少原子所有权转移协议。补齐 consuming recovery typestate与竞态测试后，B3a 可进入
实施。当前无需用户决策，故 verdict 为 `revise`。
