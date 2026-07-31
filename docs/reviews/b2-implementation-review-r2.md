---
verdict: approved
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: B2 second-round fresh-context independent implementation reviewer
date: 2026-07-31
---

# B2 第二轮独立最终实现审查

## 结论

B2 可以 RELEASE，verdict 为 `approved`。

R1 的两项重大问题均已闭合：

1. existing pin marker 现在使用 nonblocking/no-follow fd，在同一 handle 上验证 regular
   file、当前 UID、private mode、精确长度，再做固定上限读取与精确内容比较；FIFO
   timeout 测试实际证明不会阻塞；
2. failpoint 已扩展到 put、winner、blob/shard/staging barrier、pin、cleanup 和首次
  初始化/manifest 的所有逻辑提交阶段，并逐类验证失败不产生 receipt、既有权威对象
   不被覆盖、已安装对象仍可验证、cleanup failure 留下的 staging orphan 可枚举。

absolute root 逐组件 fd-relative 打开、manifest identity、receipt→audit fact、同 handle
get、并发 no-replace 等既有安全性质未发现新重大回归。B2 仍只提供 Linux 同步 Store，
不建立 Project reachability、不接入 App Service/HTTP，也不声称完成 B3/B4 或正式 MVP。

## R1 M1 — 已闭合

### 实现证据

- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:448-531`
  - pin 前先 `verify(hash)`，不存在或损坏 blob 不会得到 marker。
  - 新 marker 通过 0600 staging file、file `sync_all`、`linkat` no-replace、
    pins/staging directory fsync 提交。
  - `EEXIST` 后使用 `FILE_READ_FLAGS | OFlags::NONBLOCK` 打开 existing marker；
    `FILE_READ_FLAGS` 已含 `RDONLY|NOFOLLOW|CLOEXEC`。
  - 从同一 `File` handle 读取 metadata，要求 regular、当前 UID、无 group/world
    permission、长度精确等于 `sha256:<64hex>\n`。
  - 读取使用 `take(expected_len + 1)`，不会按攻击者文件大小无界分配；最终逐字节比较
    exact contents。
  - directory/FIFO/symlink、错误 UID/mode、超长或错误内容均 fail closed，既有 marker
    不被替换。

### 测试证据

- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:1255-1295`
  - 真实 `mkfifoat` marker 在独立线程调用 `pin()`；
  - 主线程以一秒 `recv_timeout` 证明调用返回 `BlobCorrupt` 而非等待 writer；
  - 另覆盖 4096-byte oversized marker 与 0644 weak-mode marker。
- 正常 pin 与重复 pin 仍在
  `/home/mii/code/draft/alda-agent/src/artifact_store.rs:1062-1112` 通过，证明加固没有破坏
  idempotent happy path。

因此 R1 所述 FIFO worker exhaustion 与 unbounded marker read 已被消除。

## R1 M2 — 已闭合

### put / receipt failpoint 矩阵

`StoreFailpoint` 现在覆盖：

- temp create、write、sync 前后、复读 verify 后；
- final install 前、concurrent winner verify 前、install 后；
- shard sync 前、staging unlink 前、staging sync 前；
- cleanup failure；
- pin write、install、pins sync、cleanup 与 staging sync。

对应位置：

- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:205-224`
- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:309-410`
- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:448-531`
- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:1031-1060`
- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:1154-1228`

断言语义：

- pre-install 失败返回 error、无 receipt、无 final、正常 cleanup 后无 staging temp；
- post-install/pre-directory-barrier 失败返回 error、不会签 receipt，已安装 hard-link blob
  保留并可按 hash verify；
- winner verify failpoint 不覆盖或损坏既有 winner；
- cleanup failpoint 返回 `CleanupFailed`，不签 receipt，staging orphan 可枚举；
- pin cleanup failure保留已安装 pin并留下可枚举 staging orphan。

### 初始化 / manifest failpoint 矩阵

- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:227-238` 定义 layout、staging、
  manifest file sync/install/layout sync、blobs、sha256、pins 各阶段。
- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:249-283` 在每个 logical durable
  phase 后设置 gate。
- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:728-793` 覆盖 manifest temp
  create/write/file sync、复读、no-replace winner、layout/staging sync。
- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:1231-1252` 对每一 open failpoint
  断言首次 open 失败，随后 reopen 必须恢复为可用空 Store；残留 temp 至多为可枚举
  staging orphan。

### read error

- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:1017-1023` 的 `FailingReader`
  每次 read 返回实际 I/O error。
- `/home/mii/code/draft/alda-agent/src/artifact_store.rs:1114-1151` 断言 `put` 返回
  `Io { operation: "read Artifact input" }`，无 final、无 receipt、无 staging temp。

这些测试不是仅检查枚举值；它们运行真实 temp/link/open/fsync 路径，并检查失败后的
Store 状态。对 B2 内存进程中的逻辑 barrier 门禁而言已达到获批设计要求。

## 新回归检查

- root 从 `/` fd 开始逐个 normal component
  `openat(O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)`；中间 symlink 测试 fail closed。
- root capability 取得后，即使原路径被 rename 并放入 replacement directory，后续 put
  仍落在原 inode。
- layout/shard 均由 descriptor-relative `mkdirat/openat` 创建；新目录同步 child inode
  与 parent entry。
- manifest 使用 deny-unknown-fields、regular/UID/private mode/4 KiB 上限、checksum、
  version与 stable 128-bit instance ID 校验；reopen 不重建 identity。
- put 使用 64 MiB checked streaming、expected hash/size、temp复读、hard-link
  no-replace、winner同 handle复验；八线程相同内容最终只有一个 blob。
- verify/get 不 reopen path；path 被替换后已返回 handle 仍读取原 bytes，新 verify
  检出 replacement corruption。
- `CommittedArtifactReceipt` 字段私有、无 Clone/Deserialize，只由 Store 成功路径产生；
  state consuming handoff 无损保存 layout/store/durability/commit identity 并重算。
- `ArtifactStore`、receipt 与 `ArtifactRegistered` 没有接入 wire、HTTP 下载或
  App Service production mutation；durable blob 不会自动变成 Project reachable。

## 非阻断残余

- `CleanupFailed` 当前只表达 cleanup failure，不携带原始主错误作为结构化字段；普通
  error path 对 best-effort cleanup 的二次错误也会保留主错误而不附 warning。现有测试
  已证明安全语义（无 receipt、orphan 可枚举），因此不阻止 B2，但 B4 运维诊断前应把
  primary error 与 cleanup warning 同时保留。
- failpoints 位于逻辑 barrier 前/后，并非一个可让每个底层 `fsync/linkat/unlinkat`
  syscall单独返回任意 errno 的虚拟 filesystem backend。当前矩阵已覆盖 receipt
  签发边界和恢复状态；若 B3/B4 做进程崩溃/电源故障验证，仍应加入 subprocess kill
  或 filesystem fault harness，不能把本轮进程内 failpoint 描述成真实断电证明。
- pin mode 检查采用“owner permissions 可因 umask 更窄、group/world bits 必须为零”，
  与设计的“0600 受 umask 只能更窄”一致；若未来要求 owner bits 必须精确 0600，应另行
  升级规则和测试。
- B2 同步 API 尚未进入 B4 的有界 blocking worker，这是明确后续范围。

## 机械门禁

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                  PASS
  lib unit tests                                         48 passed
  main unit tests                                         6 passed
  HTTP integration                                        1 passed
  WS integration                                          2 passed
node --check web/app.js                                  PASS
node --check web/client-state.js                         PASS
node --check web/sw.js                                   PASS
node --test web/client-state.test.js                     PASS (5 passed)
git diff --check                                         PASS
```

## RELEASE 判定

- Slice A / B1 回归：PASS。
- B2 Linux Artifact Store：**可 RELEASE**。
- B3/B4 与 Slice B 整体：未完成。
- 正式 MVP：未完成，本报告不作完成声明。
