---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: B1 second-round fresh-context independent implementation reviewer
date: 2026-07-31
---

# B1 第二轮独立最终实现审查

## 结论

B1 R1 的 M2、M3 已在实现中闭合，M1 的在线权威 mutation 入口也已显著收窄：

- `ProjectCoordinator::initialize/from_events` 已为 crate-private；
- 任意 `apply_events` 与 verified receipt registration 仅在 `cfg(test)` 编译；
- 生产 coordinator 没有接受任意 `ProjectEvent` 的入口，也无法由外部 crate 构造；
- Draft 和 Candidate 均可进入 Rejected，terminal 状态不能反向；
- 独立 v1 Project/Revision wire DTO、显式 mapper、App Service 命令及 CLI
  snapshot/list/read 均已实现。

但本轮 verdict 仍为 `revise`。R1 M1 只闭合了“不能修改 live coordinator”，没有满足
本轮明确要求的“serde facts 只存在于 trusted replay internals”：crate root 仍公开
导出 domain/state 模块，可反序列化 `ProjectEvent`/`SequencedProjectEvent`，并公开
`ProjectSnapshot::apply` 与 `replay`。这把含 `authenticated_human: bool` 和
`VerifiedDurable` tag 的事实验证 API 留给任意外部调用方，而不是封闭在未来受校验的
Event Store replay 边界。

这不是 live App Service 已可被远程篡改；当前没有这样的生产路径。但它是本轮指定的
生产编译/API 隔离条件，且会成为 B3 接入时绕过 trusted-log boundary 的现成公共入口，
因此仍阻止 B1 RELEASE。

## R1 M1–M3 修复核对

### M1 — 部分闭合：权威 coordinator 已封口，公开 serde/replay 边界仍未闭合

已闭合证据：

- `/home/mii/code/draft/alda-agent/src/state/mod.rs:518-543`：
  `initialize`、`from_events` 均为 `pub(crate)`，外部 crate 无法取得新 coordinator。
- `/home/mii/code/draft/alda-agent/src/state/mod.rs:555-568`：
  任意 `apply_events` 与 receipt registration 同时受 `cfg(test)` 和 `pub(crate)`
  限制，production compilation 不含这些入口。
- `ProjectCoordinator` 字段私有；公开 `propose` 只能在已由 crate 内部取得 coordinator
  后使用，且只构造 Revision/Evidence/BranchHead 事务，不接受任意 event。
- `VerifiedArtifactReceipt` 本身仅在 `cfg(test)` 编译；生产代码无法直接构造
  `ArtifactRegistered` 并提交给 live coordinator。

仍未闭合的位置：

- `/home/mii/code/draft/alda-agent/src/lib.rs:9-13`
- `/home/mii/code/draft/alda-agent/src/domain/mod.rs:238-245`
- `/home/mii/code/draft/alda-agent/src/domain/mod.rs:274-283`
- `/home/mii/code/draft/alda-agent/src/domain/mod.rs:325-336`
- `/home/mii/code/draft/alda-agent/src/domain/mod.rs:404-460`
- `/home/mii/code/draft/alda-agent/src/state/mod.rs:41-66`
- `/home/mii/code/draft/alda-agent/src/state/mod.rs:484-490`

实际证据：

- crate root 仍为 `pub mod domain` / `pub mod state`。
- `HumanDecision`、`ConstraintWaiver`、`ArtifactRecord`、`ProjectEvent` 与
  `SequencedProjectEvent` 都公开实现 `Deserialize`。private 字段不阻止 serde
  反序列化，外部 crate 仍可从自选 JSON 得到
  `authenticated_human=true` 或 `VerifiedDurable + commit identity` 的事实。
- `ProjectSnapshot` 的字段、`apply()` 及自由函数 `replay()` 均公开；任意外部 crate
  可以把上述 serde facts 输入完整 reducer。该 API 没有表达“批次/schema/checksum 已由
  trusted Event Store 验证”的 capability 或封闭边界。
- reducer 对 Human/Artifact 的来源校验最终仍是可序列化布尔/tag。它对合法可信日志的
  replay 是必要的，但不应同时成为任意外部调用方的公共事实入口。

影响：

- 当前 live App Service coordinator 不会因此被修改，这一点已比 R1 安全。
- 但 B1 的公共 Rust API 仍把 trusted replay 语义暴露为普通调用。B3 若沿用现有公开
  `replay/apply`，调用方可在进入 Event Store 完整性/来源验证之前让 forged human 或
  durable 事实通过 reducer；“serde facts only trusted replay internals”没有由类型或
  可见性保证。
- domain/state 内部结构也因此成为无意的公共兼容面，与已经正确建立的独立 wire DTO
  边界方向不一致。

最小修复方向：

- 将 stored fact、snapshot reducer、`replay/apply` 与 coordinator 实现收窄为
  crate-private；crate 对外只公开稳定 protocol DTO 和确有需要的应用服务接口。
- B3 接入时由一个 crate-private trusted replay facade 接受已完成 schema、sequence、
  batch checksum/来源验证的输入，再调用 reducer。不要用可序列化 bool/tag 充当在线
  capability。
- 增加外部 API/compile-time 门禁，证明 production 依赖方不能构造 coordinator、调用
  arbitrary-event mutation，也不能直接调用 stored-fact replay；crate 内 trusted
  replay 测试仍应保留完整重建能力。

### M2 — 代码语义已闭合

- `/home/mii/code/draft/alda-agent/src/state/mod.rs:281-315`：
  `RevisionRejected` 和 `RevisionAborted` 均只允许当前状态为 Draft 或 Candidate；
  Accepted/Rejected/Aborted 均 fail closed。
- `/home/mii/code/draft/alda-agent/src/state/mod.rs:954-990`：
  实测 Draft→Rejected 成功，Rejected→Aborted 被拒且 snapshot 零污染。
- Candidate→Accepted 的 VerifiedDurable/H0/Hard Gate 与 terminal replay 测试继续
  通过。

因此 R1 指出的 Draft→Rejected 行为错误已修复。

### M3 — 已闭合

- `/home/mii/code/draft/alda-agent/src/protocol.rs:35-54`：
  新增 `ProjectDomainSnapshot`、`RevisionList`、`RevisionRead` 命令。
- `/home/mii/code/draft/alda-agent/src/protocol.rs:96-141`：
  `DomainProjectSnapshotV1`、Take/Branch/Revision summary/detail 是独立 wire DTO，
  没有复用 domain snapshot/event 类型。
- `/home/mii/code/draft/alda-agent/src/app_service.rs:610-720`：
  mapper 逐字段复制白名单值；lifecycle/origin 显式映射为 wire string。
- `/home/mii/code/draft/alda-agent/src/app_service.rs:852-914`：
  三种 read command 已接入 App Service，not-found/invalid ID 使用 typed error。
- `/home/mii/code/draft/alda-agent/src/main.rs:147-170`、
  `/home/mii/code/draft/alda-agent/src/main.rs:414-448`：
  CLI 提供 Project domain snapshot 与 revision list/read。
- `/home/mii/code/draft/alda-agent/src/app_service.rs:1877-1925`：
  mapper fixture 的 JSON 不包含 `store_commit_identity`、domain event 或
  `FixtureOnly`；CLI mapping 单元测试也通过。

wire 命令/结果中未发现 `ProjectEvent`、domain `ProjectSnapshot`、
`ArtifactAvailability` 或 human capability 字段泄漏。

## 重要但非单独阻断的问题

### I1 — lifecycle 测试不是完整 Draft/Candidate 转移表

- 位置：
  - `/home/mii/code/draft/alda-agent/src/state/mod.rs:954-990`
- 证据：新增测试只直接证明 Draft→Rejected 与 Rejected→Aborted 拒绝；没有表驱动覆盖
  Candidate→Rejected、Draft/Candidate→Aborted，以及 Accepted/Rejected/Aborted 对
  Reject/Abort 的全部拒绝组合。
- 判断：实现的 `matches!(Draft | Candidate)` 本身正确且清晰，R1 的实际行为缺陷已修复，
  因此本轮不另立重大问题；但 max final gate 应补齐完整状态转换表，防止后续改动遗漏
  Candidate 或误开放 terminal 转移。

### I2 — mapper fixture 没有非空 Revision detail

- 位置：
  - `/home/mii/code/draft/alda-agent/src/app_service.rs:1877-1925`
- 证据：测试 Project 刚创建，`revisions` 与 list 都为空；它证明 DTO 结构独立，却没有
  实际执行 `RevisionRead` 或断言非空 summary/detail 的字段映射。
- 判断：Rust 类型和显式 mapper 已足以证明不会自动序列化 domain 新字段，M3 功能面已
  存在，故不阻断；建议用内部 fixture coordinator 增加非空 mapper/typed not-found
  测试，不必新增生产写命令。

### I3 — B1 read 使用 command queue/幂等缓存

三项只读命令当前走既有 `ClientCommand` command channel，而不是 App Service 的内部
query channel。现有队列仍有界，回复稳定且不会产生领域事件，因此不构成 correctness
阻断；B4 集成 Project event subscription/持久 coordinator 时应明确它们最终属于
command 还是 query 资源预算，避免高频 snapshot 占用写命令配额。

## 新回归检查

- Slice A Project create 同时、原子地在同一 `ServiceState::handle` 中创建 Slice A
  snapshot 与 B1 coordinator；初始化失败不会只插入其中一个。
- App Service 只保存一个 B1 coordinator map；未发现 wire/HTTP/CLI 可提交 B1 Revision、
  ArtifactRegistered、Waiver、Accept 或任意 ProjectEvent。
- `RevisionLifecycle` 与 wire surface 均没有 Published。
- FixtureOnly 仍不能满足 Candidate；删除 durable registration 后 replay 在 promotion
  处 fail closed。
- domain 不依赖 HTTP/Tokio/Provider/文件系统；mapper 是单向 domain projection→wire。
- B1 仍明确为内存领域核；没有把 B2–B4 durability/restart 能力误报为已实现。

## 机械门禁

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                          PASS
cargo clippy --all-targets -- -D warnings PASS
cargo test --all-targets                  PASS
  lib unit tests                          38 passed
  main unit tests                          6 passed
  HTTP integration                         1 passed
  WS integration                           2 passed
node --check web/app.js                   PASS
node --check web/client-state.js          PASS
node --check web/sw.js                     PASS
node --test web/client-state.test.js      PASS (5 passed)
git diff --check                          PASS
```

机械门禁全部通过；它们证明 R1 M2/M3 与 Slice A 回归，但没有验证 external public API
对 stored serde facts/replay 的封闭性，因此不能把最终 verdict 提升为 `approved`。

## RELEASE 判定

- Slice A 回归：PASS。
- B1 R1 M2/M3：PASS。
- B1 R1 M1：live coordinator mutation 已闭合；trusted serde replay visibility 未闭合。
- B1：**不可 RELEASE**，verdict `revise`。
- Slice B / 正式 MVP：未完成，本报告不作完成声明。
