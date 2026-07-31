---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: B3a fresh-context independent implementation reviewer R2
date: 2026-07-31
---

# B3a 独立实现审查 R2

## 结论

B3a 当前仍不可 RELEASE，verdict 为 `revise`。

R1 的 M1–M3 已闭合：stored codec现在只反序列化 primitive/codec-owned enum并逐字段
重入domain构造器；Project identity由Ready移入Poisoned并由`recover(self)`内部传递；
manifest、events、checkpoint及所有受管目录均采用same-handle类型/owner/private mode
验证，特殊文件以`NONBLOCK|NOFOLLOW`打开，有界对象限制读取长度。

R1 M4的大部分也已闭合：append真实sync-error逻辑点、checkpoint完整阶段、repair
rescan/truncate/file-sync/dir-sync、目录/manifest/events初始化阶段均有错误返回和reopen/
lease结果断言。但 events 文件的新建 durability barrier仍缺一环：创建后只同步父目录，
未先同步文件；相应file-sync failpoint和测试也不存在。这使首次已成功append后的
`events-v1.jsonl`目录项持久性没有被协议证明，是本轮唯一重大问题。

## 重大问题

### M1 — events文件创建缺少file fsync，初始化故障矩阵也遗漏该barrier

- 严重度：重大。
- 位置：
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:258-276`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1422-1446`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1750-1803`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1182-1203`
- 实际证据：
  - `open_or_create_events`执行`openat(... O_CREATE ...)`、same-handle验证后，直接
    `fsync(directory)`并返回；没有`file.sync_all()`。
  - `InitFailpoint`只有`EventsCreate`和`EventsDirectorySync`，没有
    `EventsFileSync`；初始化测试矩阵与之相同。
  - 首次append虽然最终`sync_all(events)`，但成功路径不会在该文件同步之后再次同步
    Project目录。因此新文件的持久化顺序是`dir sync → later file sync`，而不是创建
    协议所需的`file sync → parent dir sync`。
- 影响：
  - Store可以在events目录项尚未经过正确顺序的双barrier证明时签发Ready writer；首次
    append返回成功后，逻辑上仍缺少“文件内容/inode与目录项共同持久”的证据。
  - 崩溃恢复可能丢失整个权威log目录项，而API此前已把batch作为committed返回，违反
    §15“fsync后的batch才提交”和“log create与各级sync”的durability契约。
  - 当前初始化测试只能证明目录sync错误会返回错误，不能证明events file sync错误不会
    签发Ready或隐藏未持久化创建。
- 最小修复方向：
  - 区分events文件是新建还是既存；新建时same-handle验证后执行
    `file.sync_all() → fsync(Project dir)`，两步成功后才返回writer。
  - 增加`EventsFileSync` failpoint，并断言首次open返回`Io`、不签发Ready；drop/reopen
    只能得到明确可恢复状态。保留现有directory-sync错误断言。

## R1 M1–M4闭合核验

### R1 M1 — 已闭合

- `project_codec.rs`的可反序列化字段为`String`、整数、`Vec`、`Option`和codec-owned
  enum，不再直接Deserialize domain ID/hash/schema/scope/revision/evidence值。
- conversion逐字段调用ID/hash parse、`SchemaVersion::new`、scope转换、
  `CreativeBrief::validate`、`EvidenceEnvelope::validate`、artifact audit与trusted
  replay构造器。
- codec单测与checksum重算后的batch测试覆盖非法ID/hash、零schema、nested scope、
  revision parent及evidence constraint ID，并确认reducer前/中fail closed且state不前移。

### R1 M2 — 已闭合

- `PoisonedProjectWriter`保存原`DomainProjectId`；`recover(self)`不再接受调用者identity。
- Ready→Poisoned移动同一Project ID，recover/repair均先用lease key核对内部identity；
  RepairRequired/Corrupt继续持有同一lease。
- partial-tail测试直接断言poisoned identity、lease match、恢复全过程第二open拒绝，
  repair后仍只写回原Project；API已不存在identity substitution入口。

### R1 M3 — 已闭合

- root/layout/projects/Project目录均在opened fd上验证directory、current UID及无
  group/world权限。
- manifest、events、checkpoint均以`NOFOLLOW|NONBLOCK`打开，并在同一handle上验证
  regular/current UID/private mode；manifest 64 KiB、checkpoint 1 MiB有界读取，
  events按1 MiB line streaming bound扫描。
- FIFO/directory/symlink/weak-mode/oversized fixture覆盖manifest、events与checkpoint；
  checkpoint异常作为缓存丢弃并由权威log恢复。

### R1 M4 — 部分闭合，仍被上述M1阻断

- append `FileSyncError`明确返回`AppendFailure::Poisoned`，不返回success；lease仍阻止
  第二writer，recover重扫后同command返回exact reply且不重复append。
- repair四个failpoint均断言不能返回Ready、失败typestate继续持lease、drop后reopen只见
  committed prefix或明确RepairRequired。
- checkpoint六个阶段均断言错误返回且reopen得到相同权威projection。
- 初始化目录/manifest/Project/events create和现有sync点均断言首调用`Io`且后续状态
  可明确reopen；但events file-sync操作、failpoint和证据仍缺失。

## 新回归检查

未发现新的重大回归：

- stable reply仍保存并返回exact canonical raw bytes，command index键与digest冲突语义
  未被修复改动破坏。
- complete newline坏行仍fail closed；incomplete final tail仍必须持lease compare-and-
  truncate repair。
- checkpoint仍从log prefix验证anchor，异常checkpoint不会替代权威log。
- primitive codec没有重新公开live Human/Artifact capability或domain Deserialize入口。

## 机械门禁

在`/home/mii/code/draft/alda-agent`实际运行：

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                  PASS
  lib unit tests                                         65 passed
  main unit tests                                         6 passed
  HTTP integration                                        1 passed
  WS integration                                          2 passed
node --check web/app.js                                  PASS
node --check web/client-state.js                         PASS
node --check web/sw.js                                   PASS
node --test web/client-state.test.js                     PASS (5 passed)
git diff --check                                         PASS
```

## RELEASE判定

- R1 M1：PASS。
- R1 M2：PASS。
- R1 M3：PASS。
- R1 M4：PARTIAL；events create file-sync barrier与故障证据未闭合。
- B3a：**不可 RELEASE**。
- 本报告是max round 2最终复核；剩余重大问题需由主流程裁决。
