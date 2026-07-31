---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B2 fresh-context independent design reviewer
date: 2026-07-31
---

# B2 独立设计审查

## 结论

B2 的基本提交顺序正确：随机 `create_new` staging、stream hash/size、temp
`sync_all`、复读校验、no-replace 安装、winner 校验、目录同步后才签发 receipt；同时
明确 blob 存在不等于 Project reachability，B4/B3 才形成公开
`ArtifactRegistered`。同 handle verify/get、无自动 GC 和 failpoint 方向也符合 MVP。

但当前设计不能批准实施，verdict 为 `revise`。存在四项会破坏 symlink/TOCTOU 安全、
Linux power-loss durability 或 receipt 可信链的重大问题：

1. “检查路径不是 symlink”与后续基于路径的 open/create/rename 分离，目录组件可被竞态
   替换；
2. `FileAndDirectorySynced` 没有定义首次创建完整目录链及 store identity manifest 的
   持久化协议；
3. “domain receipt 构造器只给 sibling `artifact_store`”无法用当前 Rust 模块可见性
   直接表达；
4. receipt 的 store/layout/durability 审计字段没有进入当前 B1
   `ArtifactRegistered`/`ArtifactRecord`，消费时会丢失 B3 必需事实。

这些问题必须在代码和 v1 事实 schema 冻结前解决，不要求 B2 提前实现 B3 Event Log 或
B4 Project reachability。

## 必须修复项

### M1 — symlink 检查不是 anchored filesystem capability，存在目录替换 TOCTOU

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:899-919`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:925-941`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:955-963`
- 实际证据：
  - 设计要求 open/operation 时拒绝 symlink，并要求 final 使用 no-follow regular-file
    打开；但所有布局与操作仍描述为从 root 拼 path 后执行 `create_new`、open、link/
    rename。
  - 没有冻结 root/`artifacts-v1`/`blobs`/shard/staging/pins 的 directory handle，
    也没有要求后续 lookup 相对这些 handle 使用 `openat`/`openat2`、`O_NOFOLLOW` 和
    component identity 校验。
  - 因此在“检查 shard 不是 symlink”和“创建/打开 final”之间，另一个本地进程可以把
    shard 或其父目录 rename 后换成 symlink；对 final 自身使用 no-follow 不能阻止父
    组件逃逸。verify/get/pin/list 同样受影响。
- 影响：
  - Store 可在授权 data root 外创建、覆盖或读取 hash 命名文件，违反 MVP 的路径根与
    symlink 逃逸硬约束。
  - 仅依赖 0700 不足以形成证明：现有 root 的 owner/mode 未冻结，B4 实例锁尚未实现，
    崩溃恢复或同用户恶意进程仍可替换目录。测试中“预置 symlink”也无法发现竞态。
- 最小修复方向：
  - Linux 首发冻结 descriptor-relative 实现：安全打开并持有 root/layout/staging/
    blobs/pins directory handles，所有 component lookup/create/link/rename 相对可信
    handle 执行，拒绝 symlink/non-directory，并验证目录 device/inode 不被替换。
    可选用经过审计的 `rustix`/`cap-std`/等价封装；不能用 canonicalize 后再普通 open。
  - 若 Windows 无法提供等价 reparse-point/handle-relative保证，明确返回
    `UnsupportedSafety`/降级 capability，不能只降低 durability capability却仍声称
    path-safe。
  - 增加可控 race 测试：在检查与 open/install/verify/pin/list 间替换父目录，操作必须
    fail closed 或继续绑定原 handle，绝不能访问替换目标。

### M2 — `FileAndDirectorySynced` 未覆盖完整新建目录链及持久 store identity

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:903-918`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:925-946`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:948-950`
- 实际证据：
  - receipt 包含随机 store instance ID、layout version 和 durability capability，但
    设计没有说明 store instance ID 存在哪里、是否只生成一次、如何原子创建/校验及
    如何跨 reopen 保持稳定。
  - `open` 可能首次创建 `artifacts-v1/blobs/sha256/staging/pins`；put 还会创建 shard。
    设计只写“创建/同步 shard”“sync shard/blobs dir”，没有规定每个新目录创建后同步
    其自身及父目录，亦未说明 root/layout/sha256/staging/pins 的初始化提交点。
  - Linux 上仅同步 final inode 和末级 shard 不能保证掉电后完整目录链或 store identity
    仍存在。一个 receipt 若宣称 `FileAndDirectorySynced`，必须覆盖从既有 durable
    ancestor 到 final entry 的全部新增目录项。
- 影响：
  - put 返回成功 receipt 后断电，blob 或承载它的 shard/layout 目录可能消失；B3 已
    持久化的 `ArtifactRegistered` 将引用不存在的 blob。
  - 若 store ID 每次 open 随机生成，同一 Store 的 commit identity 跨重启不稳定；若
    identity manifest 写一半或被替换，receipt audit vector 不可信，unknown layout
    也无法可靠 fail closed。
- 最小修复方向：
  - 冻结版本化 store manifest（layout version、随机 instance ID、capability/version）
    的 create-new temp→sync→no-replace install→parent sync 协议；reopen 必须校验
    regular/no-follow、内容与权限，不重新生成 identity。
  - 明确初始化和 lazy shard 创建的逐级 fsync 顺序，包括每个新 directory inode 与其
    parent directory entry；rename 跨 staging/shard 时说明 source 与 destination
    directories 各自何时同步。
  - receipt 只有在 manifest、完整目标目录链、final inode 与 final entry 都达到平台
    声明的 durability 后才能签发。failpoint 覆盖首次 open 的每一级 mkdir/manifest
    install/sync，而不只覆盖 put。

### M3 — 当前 Rust 模块结构无法保证 receipt 生产构造权只属于 sibling `artifact_store`

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:896-897`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:944-949`
  - `/home/mii/code/draft/alda-agent/src/lib.rs:9-13`
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:371-402`
- 实际证据：
  - 当前 `domain` 与未来 `artifact_store` 是 crate root 下的 sibling modules。
    `VerifiedArtifactReceipt` 当前仅在 tests 编译，字段私有。
  - Rust 的 restricted visibility 只能授权给声明模块的 ancestor path；在
    `domain` 内不能写出“只允许 sibling `crate::artifact_store` 调用”的 constructor。
    `pub(crate)` 会把构造权交给 app_service/http/state 等整个 crate；保持 private 则
    artifact_store 也无法构造。
  - 设计只写“生产构造器仅 artifact_store 可调用”，没有冻结能实际实现这一性质的
    类型所有权或 sealed factory 结构。
- 影响：
  - 直接实现时要么编译不通，要么退化为任意 crate module 都能铸造
    VerifiedDurable receipt，重新打开 B1 已修复的 capability bypass。
- 最小修复方向：
  - 将 receipt 的不可伪造生产类型归 `artifact_store` 所有，只公开只读 accessor 和
    consuming handoff；由 `state` 接受该 opaque 类型。或者把 domain/state/store 放入
    一个共同私有父模块，用私有 sealed token/factory 让 Store 成为唯一 producer。
  - 不要用可设置 bool/enum、公开 `Deserialize`、公开字段或仅靠命名表达 capability。
    增加 crate 内 compile-fail/API visibility 测试或外部集成测试，证明非 Store
    production module 无法构造 receipt。

### M4 — receipt 审计字段与 B1 durable fact 不匹配，消费会丢失 durability 证据

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:944-949`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:779-800`
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:331-369`
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:371-400`
- 实际证据：
  - B2 receipt 要保存 canonical hash、size、layout version、store instance ID、commit
    identity 与 durability capability，并明确 B3 stored fact 保存审计字段。
  - 当前 B1 `ArtifactRecord` 只有 hash、size、availability 与 commit identity；没有
    layout version、store instance ID 或 durability capability。
  - 当前 `into_record` 消费 receipt 后只产生上述四项字段。若 B2 只扩展 receipt 而不
    先版本化 B1 durable fact，这些审计字段会在生成 `ArtifactRegistered` 时丢失。
- 影响：
  - B3 replay 只能看到一个无法归属 Store 实例、布局或实际 durability policy 的
    `VerifiedDurable` 标签，不能证明 Linux `FileAndDirectorySynced` 与 Windows
    `FileAndRename` 的差异，也无法审计 commit identity 的 canonical input。
  - 后续给 v1 `ArtifactRegistered` 增字段会形成持久 schema 迁移；旧事件无法补造当时
    的真实 capability。
- 最小修复方向：
  - 在 B2 实施前冻结版本化 `ArtifactRegistered` audit record：至少 layout/store
    instance/durability capability/hash/size/commit identity，并让 commit identity 的
    canonical bytes 有固定测试向量。
  - `VerifiedArtifactReceipt` 的 consuming conversion 必须逐字段无损生成该 fact；
    Project reachability 仍只由 B4/B3 commit 建立，不能因字段扩展而让 B2 自行公开。

## 非阻断残余

- 同 hash 并发 put 的 no-replace winner/loser 设计基本正确：loser 校验 winner 后才能
  去重，不能覆盖。实施时应保证 loser 自己也完成声明所需的 final/dir durability 后才
  返回 receipt。
- 同 handle verify→rewind→get 正确解决 final path 被替换的 TOCTOU；M1 要求把相同
  handle-relative原则扩展到所有父目录组件。
- B2 不建立 Project reachability、不改变 A3 下载源是正确范围；B4 之前即使 blob
  durable 也不得通过 HTTP/CLI manifest 自动可见。
- 64 MiB 上限、checked `u64`、0-byte、expected hash/size、read error 和 corruption
  复验足以形成 B2 streaming 基线。是否以后按 Artifact kind 配置不同上限可后续决定。
- pin 使用 hash-only marker、verify-before-pin、无 unpin/delete/GC 符合 MVP；marker
  的具体版本化内容和重复 pin audit 可在实施中冻结，不影响 blob commit协议。
- cleanup failure 需要区分“主 put 已失败”和“final 已 durable、仅 staging unlink
  失败”的返回结构；只要绝不撤销或伪造 receipt、并把残留列为 staging orphan，可作为
  typed warning 细化，不单独阻断设计。

## 审查判定

修订 M1–M4 后可进入第二轮设计复核。B2 无需提前实现 Event Log、Coordinator CAS 或
公开下载，但必须先给 B3/B4 提供一个路径安全、掉电语义准确且不可伪造、审计字段无损的
Store receipt。
