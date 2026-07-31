---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: B2 fresh-context independent implementation reviewer
date: 2026-07-31
---

# B2 独立最终实现审查

## 结论

B2 当前不可 RELEASE，verdict 为 `revise`。

Linux 实现的主路径与获批设计基本一致：absolute root 从 `/` fd 逐组件
`openat(O_DIRECTORY|O_NOFOLLOW)`，root 以下全部操作 descriptor-relative；新目录同步
inode 与 parent；manifest identity 持久且校验 checksum；put 使用同步 temp、
复读 hash/size、`linkat` no-replace、winner 复验和 shard/staging fsync；get 使用同一
handle；receipt opaque 且 audit 字段无损进入 B1 fact；没有接入 App Service 下载或自动
Project reachability。

但仍有两项重大阻断：

1. 已存在 pin marker 未验证 regular file、owner/mode 和长度，FIFO 可让同步 `pin()`
   永久阻塞，超大文件可造成无界内存读取；
2. 实际 failpoint/故障测试远少于获批 B2 durability 门禁，关键目录/manifest/write/
   winner/dir-sync/cleanup 失败无法注入，无法证明“任何失败都不签 receipt”。

## 重大问题

### M1 — pin 的 `EEXIST` 路径会跟随非 regular 对象语义，FIFO 可阻塞 Store

- 位置：
  - `/home/mii/code/draft/alda-agent/src/artifact_store.rs:394-452`
  - `/home/mii/code/draft/alda-agent/src/artifact_store.rs:780-791`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:983-986`
- 实际证据：
  - 新 pin 安装遇到 `EEXIST` 后，用
    `openat(..., O_RDONLY|O_NOFOLLOW|O_CLOEXEC)` 打开 marker，随后直接
    `read_to_string`。
  - 该路径没有像 blob/manifest 那样先以 handle metadata 验证 regular file、当前用户、
    mode 0600 和最大固定长度。`O_NOFOLLOW` 只拒绝 symlink，不拒绝 FIFO、device 或
    其他特殊文件。
  - Linux 上以 blocking `O_RDONLY` 打开 FIFO 会等待 writer；即使是 regular file，
    `read_to_string` 也没有大小上限。`pins` 是当前用户 0700，但同 UID 的另一进程仍可
    预置/替换 marker，B4 实例锁尚未实现且不能替代 Store 自身 fail-closed。
  - 现有 pin 测试只覆盖正常重复 marker和安装前 failpoint，没有 directory/FIFO/
    oversized/corrupt-mode/owner marker。
- 影响：
  - 一个异常或同用户恶意 pin entry 可永久阻塞同步 Store worker；B4 将 Store 放入有界
    blocking pool 后仍会耗尽 worker，形成稳定 DoS。
  - 无界 marker 读取还可造成内存放大，违反所有资源有界与非 regular fail-closed
    契约。
- 最小修复方向：
  - existing marker 打开时加入适合的 nonblocking/no-follow 策略，并在读取前从同一
    handle 验证 regular file、owner、private mode 和精确/小上限 size；然后只读取固定
    长度并精确比较内容。
  - 非 regular、symlink、错误 owner/mode、过大或内容不符统一 fail closed，且不替换
    既有 marker。
  - 增加 FIFO、directory、symlink、oversized regular、wrong contents/mode 的测试，
    FIFO 用超时证明 `pin()` 不会挂起。

### M2 — failpoint 表面不足，关键 power-loss/error 路径没有确定性证据

- 位置：
  - `/home/mii/code/draft/alda-agent/src/artifact_store.rs:201-220`
  - `/home/mii/code/draft/alda-agent/src/artifact_store.rs:261-266`
  - `/home/mii/code/draft/alda-agent/src/artifact_store.rs:304-353`
  - `/home/mii/code/draft/alda-agent/src/artifact_store.rs:1024-1082`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:995-1016`
- 实际证据：
  - 实现只有四个 put/pin failpoint：
    `AfterTempSync`、`BeforeBlobInstall`、`AfterBlobInstall`、
    `BeforePinInstall`；open 只有 `AfterLayoutSync` 和
    `AfterManifestInstall`。
  - 获批设计要求初始化每一级 mkdir/dir sync、manifest write/install/layout sync、
    temp create/write/sync、after verify、concurrent winner installed、final
    directory sync、pin temp/write/install 和 cleanup failure 均可注入。
  - 当前测试不能让 `ensure_directory` 的 child/parent fsync、manifest file sync、
    winner validation、shard fsync、staging unlink/fsync、pins fsync 或 cleanup
    单独失败。`AfterBlobInstall` 发生在 shard fsync 前，但不等于验证“shard fsync
    本身返回错误时不签 receipt”。
  - `put()` 在错误后忽略 cleanup unlink/fsync 的错误，没有设计要求的 typed cleanup
    warning，也没有 cleanup failure 后 staging 可枚举的测试。
- 影响：
  - B2 的核心产品是 durability receipt；如果无法对每个 receipt 前 I/O barrier 注入
    失败，就没有证据证明某个漏掉的 `?`、错误映射或 cleanup 分支不会在失败后错误签发
    receipt、破坏既有 blob 或隐藏 staging orphan。
  - 普通 happy-path 测试与进程内 `After*` 模拟不能替代目录同步/cleanup 错误，尤其是
    B3 将据 receipt 持久化 `VerifiedDurable` 后无法回滚。
- 最小修复方向：
  - 把文件系统原语收口到 test-injectable backend/failpoint wrapper，至少完整覆盖获批
    清单中的 create/write/sync/link/open-winner/unlink/dir-sync 操作。
  - 每个点断言：不返回错误 receipt、existing final/manifest/pin 不被覆盖、成功安装的
    orphan仍可 verify、失败 staging 可枚举且 cleanup warning 不丢失。
  - 增加 read-error reader、concurrent winner failpoint 和首次初始化逐级 failure
    matrix；这些是 B2 RELEASE 门禁，不应推迟到 B4。

## 已验证通过

- root 从 `/` fd 开始逐个 normal component `openat`，拒绝空、`.`、`..`、symlink 与
  non-directory；取得 root 后路径整体 rename/替换不改变 Store capability。
- 新目录使用 `mkdirat`，随后同步 child inode 与 parent entry；现有目录验证 owner 和
  private mode。非 Linux明确返回 `UnsupportedSafety`。
- manifest 使用 staging temp、file sync、复读/校验、`linkat` no-replace、
  layout/staging fsync；instance ID 跨 reopen 稳定，未知字段/version/checksum
  fail closed。
- put streaming 上限 64 MiB，checked size，temp sync 后同 handle 复读 hash/size，
  requested hash/size mismatch 在 final install 前拒绝。
- 同 hash 并发 put 用 `linkat` winner/loser；loser no-follow 打开并复验 winner，
  不覆盖 final；8-thread 测试得到单一 blob。
- final 安装后同步 shard，staging unlink 后同步 staging，随后才创建 receipt。
- verify/get 从持有 shard fd no-follow 打开；get 验证、rewind 并返回同一 file handle，
  path 替换不改变已返回内容。
- `CommittedArtifactReceipt` 无 Clone/Deserialize、字段私有且只能由 Store 产生；
  state consuming handoff生成含 hash、size、layout、instance、durability 与 canonical
  commit identity 的 audit fact，reducer 会重算校验。
- 全仓没有 ArtifactStore/receipt 接入 `AppService`、HTTP Artifact 下载或 wire 命令；
  B2 blob 不会自动成为 Project 可见 Artifact。

## 非阻断残余

- Existing blob verification检查 regular、private mode、size 和 hash，但没有显式检查
  blob owner UID。由于所有受管父目录要求当前用户所有且 0700，正常非特权路径不能由
  其他 UID 注入；建议仍与 manifest/root 规则统一检查 owner，但本轮不单独阻断。
- `put()` 在 final 已 durable、仅 staging unlink 失败时返回错误而不是 receipt；
  重试可安全 dedup。最终 API 应通过 typed cleanup warning区分该状态，已并入 M2 的
  测试/错误面要求。
- Store 是同步 API，尚未接入 B4 的有界 blocking worker；这是明确 B4 范围，不阻止
  B2。
- B2 不存 MIME、producer、Revision、工具版本等 Project metadata；这些属于 B3/B4
  `ArtifactRegistered` 事务事实，不应误塞入 content-addressed blob Store。

## 机械门禁

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                                      PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features               PASS
  lib unit tests                                      46 passed
  main unit tests                                      6 passed
  HTTP integration                                     1 passed
  WS integration                                       2 passed
node --test web/client-state.test.js                  PASS (5 passed)
node --check web/app.js                               PASS
node --check web/client-state.js                      PASS
node --check web/sw.js                                PASS
git diff --check                                      PASS
```

机械门禁通过不覆盖 M1 的 blocking special-file路径，也不能替代 M2 缺失的真实 I/O
故障矩阵，因此 verdict 仍为 `revise`。

## RELEASE 判定

- Slice A / B1 回归：PASS。
- B2 主存储路径：主体正确，但 M1/M2 未闭合。
- B2：**不可 RELEASE**。
- Slice B / 正式 MVP：本报告不作完成声明。
