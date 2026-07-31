---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B2 second-round fresh-context independent design reviewer
date: 2026-07-31
---

# B2 第二轮独立设计审查

## 结论

B2 R1 的 M2、M3、M4 已闭合，M1 在 root fd 取得后的全部 Store 操作上也已闭合：
layout、staging、blob、shard、pin、list 与 unlink 都固定为 descriptor-relative，
no-replace `linkat`、同 handle verify/get、目录逐级 fsync、持久 manifest identity、
Store-owned opaque receipt 与完整 `ArtifactRegistered` audit fact 均形成可实施契约。

但 verdict 仍为 `revise`。M1 还剩一个位于 capability 起点的重大缺口：
`ArtifactStore::open(root)` 对绝对 root 只规定
`open(..., O_DIRECTORY|O_NOFOLLOW)`；Linux `O_NOFOLLOW` 只保护最后一个路径组件，
绝对路径的中间组件仍可跟随 symlink。因而后续所有安全的 `*at` 操作可能被锚定到一个
经中间 symlink 解析到的非预期 root。

修复不需要改变 Store 布局或放弃 `rustix`。应冻结从可信祖先取得 root fd 的方法：
逐组件 `openat(O_DIRECTORY|O_NOFOLLOW)`，或 Linux `openat2` 加
`RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS`（必要时再加 mount policy）；不支持该安全
原语时按现有策略返回 `UnsupportedSafety`。

## R1 M1–M4 闭合核对

| R1 项 | 判定 | 第二版证据 |
|---|---|---|
| M1 fd-relative / race | **部分闭合，仍阻断** | §14:915-921 已把 root 以下全部组件和操作绑定已持有 fd，并要求 race 继续绑定原 inode 或 fail closed；但最初绝对 root 的中间组件仍由普通 `open` 解析，详见 M1。 |
| M2 完整目录与 manifest durability | 已闭合 | §14:946-953 冻结 manifest create/sync/linkat/layout sync、稳定 instance ID、逐级新目录 inode + parent entry fsync，以及 lazy shard 同步；§14:943-944 要求全链与 final 达标后才签 receipt。 |
| M3 opaque receipt ownership | 已闭合 | §14:955-960 将 receipt 类型归 `artifact_store` 所有，字段私有、无 Deserialize/Clone，只能由成功 put 返回；state 仅 consuming handoff。该结构不再依赖 Rust 无法表达的 sibling-only constructor visibility。 |
| M4 audit fact 完整性 | 已闭合 | §14:962-966 要求 v1 fact 无损保存 hash、size、layout version、store instance ID、durability capability、commit identity，并重算 canonical identity；B3 保存审计字段而非 opaque capability，B4 才建立 reachability。 |

## 必须修复项

### M1 — 初始 root fd 不是从可信祖先逐组件取得，绝对路径中间 symlink 仍可逃逸

- 位置：
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:899-900`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:915-924`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1013-1015`
- 实际证据：
  - 设计只说 root 以 `O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC` 打开，没有规定从哪个已信任
    directory fd 开始，也没有规定绝对路径的每一个父组件如何解析。
  - Linux `O_NOFOLLOW` 只拒绝 trailing/basename 是 symlink；早先路径组件仍会跟随
    symlink。[Linux `open(2)`](https://man7.org/linux/man-pages/man2/open.2.html)
    明确说明这一点。
  - 因而 `/trusted/user/data-root` 中若 `user` 或其他中间组件在 lookup 时被替换为
    symlink，`open(root, O_NOFOLLOW)` 可以成功取得 symlink 目标下的真实目录 fd。
    root 本身的 owner/mode 检查只能验证目标目录，不能证明它是 composition root
    指定路径层级中的那个目录。
  - root fd 一旦取得，§14 后续的 `mkdirat/openat/linkat/unlinkat` 确实不会再被路径
    替换带走；问题只在初始锚点可能已错误。
- 影响：
  - Store 可以在授权 data-root 层级之外创建或读取 Artifact，同时仍满足目标目录
    owner/mode 检查。
  - 预置 “root 本身是 symlink” 测试会通过 fail-closed，但无法覆盖“中间父组件是
    symlink”或解析期间替换；因此当前验收会对完整 anchored-path safety 产生假阳性。
- 最小修复方向：
  - 冻结可信 ancestor（例如由 composition root 已安全持有的 private data-parent fd），
    再逐个验证的相对组件使用 `openat(O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC)` 取得 root；
    不接受多组件字符串直接进入普通 `openat`。
  - 或使用 Linux `openat2`，以可信 dirfd 和
    `RESOLVE_NO_SYMLINKS|RESOLVE_NO_MAGICLINKS` 解析 root；若还要禁止 mount/bind-mount
    边界，应显式决定是否加 `RESOLVE_NO_XDEV`，不要默认为已有保证。
  - 若 kernel/rustix 路径无法提供所冻结的语义，返回 `UnsupportedSafety`；不要回退到
    `canonicalize + open` 或普通绝对 `open`.
  - 验收增加 root 中间组件 symlink、父组件 rename/symlink race，以及安全取得 root fd
    后路径整体替换的三类测试。

## rustix / linkat 可实现性核验

所需方案在当前 Linux/rustix API 上可实现，不构成 blocker：

- `rustix::fs` 提供 `openat`、`openat2`、`mkdirat`、`linkat`、`renameat`、
  `unlinkat`、`statat` 和 directory iteration 所需原语；`openat` 返回 `OwnedFd`，
  可持续作为 capability 持有。[rustix fs API](https://docs.rs/rustix/latest/rustix/fs/)
- `OFlags::NOFOLLOW` 可用于 final component，`openat2` 的 resolve flags用于全路径
  component policy；Linux 文档明确 `RESOLVE_NO_SYMLINKS` 覆盖所有组件，而
  `O_NOFOLLOW` 只覆盖最后组件。[Linux `openat2(2)`](https://www.man7.org/linux/man-pages/man2/openat2.2.html)
- `linkat(old_dirfd, old_name, new_dirfd, new_name, flags)` 正好支持 staging/shard 两个
  已持有 fd；Linux `linkat` 在目标已存在时不会覆盖，返回 `EEXIST`，适合 winner/loser
  no-replace 安装。[Linux `linkat(2)`](https://www.man7.org/linux/man-pages/man2/link.2.html)
- staging 与 shard 若不在同一 filesystem，`linkat` 会失败；当前布局位于同一
  `artifacts-v1` 树，实施仍应把 `EXDEV` 作为 fail-closed I/O/unsupported-layout
  错误，而不能复制后冒充原子提交。

## 已闭合的关键契约

- 新目录 durability：每级 `mkdirat` 后同步 directory inode，再同步 parent entry；
  Store open 在整条布局完成前不成功。
- manifest：版本化 canonical 内容、随机稳定 instance ID、checksum、temp file sync、
  no-replace install、layout/staging sync 与并发 winner 校验均已定义。
- put：temp data fsync + 复读 hash/size、expected 校验、hard-link no-replace、
  concurrent winner 校验、final/shard/staging sync 后才返回 receipt。
- dedup loser：复用已 fsynced temp inode对应 winner blob并自行同步 shard，不能仅看到
  文件名就签 receipt。
- get/verify：同一 no-follow regular handle 验证、rewind、返回，不存在 path reopen
  TOCTOU。
- opaque handoff：receipt 不能 Clone/Deserialize，Store 是唯一 producer；state 消费
  后才生成完整 audit fact。
- reachability：B2 blob/receipt 不等于 Project 可见；B4/B3 transaction 才能持久化
  `ArtifactRegistered`。
- 非 Linux 明确 `UnsupportedSafety`，没有用较弱实现冒充相同 capability。

## 非阻断项

- “Store 初始化必须先 durable commit manifest”与 manifest temp 位于 staging 的文字
  次序略显含糊；实施显然需要先安全创建并同步 layout/staging，再提交 manifest，之后
  完成其余布局并成功返回 open。建议把“先”表述为“在 Store 对外可用前”，但完整 fsync
  条件已写清，不单独阻断。
- `pin` 段先写“no-replace rename”，随后指定实际采用 `linkat`。实施应统一术语为
  hard-link no-replace，避免误用会覆盖的普通 `renameat`；实际原语与同步集合已经明确。
- manifest 的 canonical serialization、checksum input、instance ID 编码及 commit
  identity 应提供固定 byte vector；这些已进入验收方向，可在实现门禁中冻结。
- owner/mode 检查还应使用 fd metadata而不是 lookup path metadata；§14 的
  descriptor-relative与逐组件验证要求已足以导出该实现，不需新增设计轮次。

## 审查判定

M2–M4 已闭合，M1 也只剩 root capability acquisition。补齐从可信祖先到 root 的
全组件 no-symlink 解析及对应 race 测试后，B2 可进入实施。当前无需用户决策或外部条件，
故 verdict 为 `revise`。
