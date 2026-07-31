---
verdict: approved
scope: final
artifact: /home/mii/code/draft/alda-agent
---

# A3 独立最终实现审查

## 结论

批准。未发现需要阻断 A3 RELEASE 的重大问题。

当前实现满足 A3 设计冻结的核心边界：blob、occurrence manifest 与 Project
reachability 分离；同一确定性 fixture 在 blob 层去重，每个不同成功 Turn 保留真实
provenance；HTTP 下载通过现有 bounded App Service actor 解析，不共享 Artifact
Store；fixture preparation 失败发生在事实、Store 和稳定 reply 写入之前；下载先验证
认证、Project reachability、hash/size corruption，再判断 ETag；路由和 wire 均不接收
文件路径、文件名或 filesystem target。

A3 仍明确是进程生命周期内的 Fake fixture slice，不是持久 Artifact Store、Revision、
MIDI/音频或已通过 Alda parse Gate 的作品。磁盘 staging、fsync/rename、replay 和
orphan 清理留给 B，符合已批准范围，不构成本轮缺陷。

## 审查范围与一手证据

本轮独立阅读并交叉核对：

- `docs/requirements/product-requirements.md`
- `docs/design/mvp-design.md` 的 Artifact、统一 App Service、权限和安全设计
- `docs/plans/mvp-deliberative-execution.md` 的 A2 RELEASE 加固项及 A3 §9
- `docs/reviews/a3-design-review.md`
- `docs/reviews/a3-design-review-r2.md`
- `alda-agent/Cargo.toml`、`Cargo.lock`、`README.md`
- `alda-agent/src/{lib,protocol,app_service,http,main}.rs`
- `alda-agent/tests/http_round_trip.rs`
- 当前 `git diff`、工作树状态及源码路径/Store 检索结果

## 对抗检查结果

### blob、occurrence、provenance 与 Project 隔离

通过。

- `ServiceState` 分别持有 `blobs`、`occurrences` 和 `reachability`
  （`src/app_service.rs:329-331`），不存在把单一 provenance 塞入 hash-keyed blob
  的旧设计冲突。
- 每次不同成功 Turn 都分配新的 occurrence ID，manifest 记录 owning Project、
  source Session/Turn 和 created sequence（`src/app_service.rs:1004-1035`）；blob
  使用 hash entry 去重，Project reachability 使用 `(ProjectId, ArtifactHash)`。
- `artifact_occurrences_preserve_provenance_while_blobs_deduplicate` 实际覆盖同 Project
  两个 Turn 和跨 Project 相同 bytes：最终为 1 blob、3 occurrences、2 reachability，
  且来源 Turn 与 occurrence 均不同（`src/app_service.rs:2644-2692`）。
- manifest 查询以 occurrence 为主键并再次核对 owning Project；跨 Project和不存在
  occurrence 均返回同一 `ArtifactNotFound`（`src/app_service.rs:604-617`,
  `2694-2741`）。

### preparation 失败与原子可观察性

通过。

- Approve 路径在取得任何 mutable Session 引用、追加事件或修改 Store 前调用
  `prepare_fixture`；hash/size mismatch 立即返回 `ArtifactPreparationFailed`
  （`src/app_service.rs:976-1003`）。
- 成功路径的 blob、reachability、occurrence、Approval/Turn facts 与 reply 构造均在
  actor 的同步处理函数内完成，没有 `await`；actor 在该 transition 中不能穿插下载
  查询（`src/app_service.rs:232-265`, `1004-1063`）。
- `ServiceState::handle` 明确不缓存 `ArtifactPreparationFailed` reply，使相同 command
  ID 在故障解除后可重新执行（`src/app_service.rs:369-410`）。
- hash 与 size 两种故障注入均断言 sequence 不增长、Turn 仍
  `WaitingForInput`、Approval 仍 `Pending`，blob/occurrence/reachability 全为零；
  清除故障后同一命令成功，证明不存在失败 reply 缓存
  （`src/app_service.rs:2744-2797`）。

### approve、deny、cancel 与无效输入边界

通过。

- 只有 `Approve` 准备并创建 Artifact；`Deny` 产生 Failed Turn 且 Store 三项计数均为
  零（`src/app_service.rs:983-996`, `2152-2187`）。
- 两阶段取消按创建序中止 pending question/approval 后再完成 Turn；两种事件流均从空
  projection replay，并逐字段等于在线 snapshot；取消后响应 pending object 返回
  `RequestOwnerTurnAborted`，且无 Artifact（`src/app_service.rs:686-801`,
  `2190-2397`）。
- invalid choice 和 approval digest mismatch 均不追加事实、不改变 pending 状态且
  不创建 Artifact（`src/app_service.rs:2400-2487`）。
- 同 command ID transport retry 返回原稳定 manifest，不重复 occurrence；不同 command
  ID 对已解决 approval 返回业务层 `ApprovalAlreadyResolved`，不重复创建
  （`src/app_service.rs:2490-2584`）。

### bounded actor 与无共享 Store

通过。

- `AppService` 只持有 bounded `mpsc::Sender<AppMessage>`；`AppServiceRunner` 独占
  `ServiceState`（`src/app_service.rs:77-99`, `227-230`）。
- wire command 与 `ResolveArtifactDownload` 共用同一 channel 和 runner
  （`src/app_service.rs:138-156`, `232-250`, `268-286`）。
- `HttpState` 只有 `AppService` 与 immutable auth；router 没有 `Arc<Mutex<_>>`、
  `RwLock` 或 Store handle（`src/http.rs:78-95`）。
- 下载返回 `Arc<[u8]>` 的只读 verified snapshot；源码检索未发现文件系统打开、
  写入、canonicalize 或共享 mutable Store 旁路。

### HTTP 路由、认证、oracle、headers、缓存、ETag 与 corruption

通过。

- 固定路由仅为 `GET /v1/artifacts/{sha256_hex}`；path 段必须恰为 64 位 lowercase
  hex，wire hash 必须为 `sha256:<64 lowercase hex>`（`src/http.rs:94-119`,
  `src/protocol.rs:191-244`）。
- 下载先执行精确 Host、Origin、bearer token 认证，再解析 Project header/hash，之后
  才向 actor 查询；未认证请求不能探测 Artifact（`src/http.rs:48-74`, `98-136`）。
- actor 先检查 `(project, hash)` reachability；不可达统一为 `NotFound`。可达后重新
  计算 bytes hash 和 size，corrupt 返回 `500`；只有完整性通过后才精确比较 quoted
  ETag（`src/app_service.rs:1066-1096`）。
- 专门的 corruption 测试携带命中 ETag，实际得到 `500` 而不是 `304`
  （`src/http.rs:266-304`）。
- `200` 设置固定 MIME、length、quoted ETag、`nosniff` 和仅由 short hash 派生的
  attachment filename；`304` 无 body。所有 Artifact `200`、`304` 和错误响应统一
  设置 `Cache-Control: private, no-store` 与
  `Vary: Origin, Authorization, X-Alda-Project-Id`
  （`src/http.rs:137-200`）。
- real loopback HTTP 测试覆盖成功 bytes/hash/headers、304、缺失/错误 Project
  header、Host、Origin、token、无效 hash、附加客户端 filename 路径以及最终
  A2 snapshot/event facts（`tests/http_round_trip.rs`）。
- Range 被显式拒绝；接口不存在绝对路径、`..`、URL、客户端 filename 或本地导出
  参数。README 的 curl 示例也只使用 manifest hash 和 Project header。

### manifest ownership、CLI、README 与 A2 加固

通过。

- occurrence manifest 的 owning Project 在 wire 查询时强制核对，跨 Project 与缺失
  使用相同错误；HTTP 则独立核对 Project reachability。
- CLI 暴露 `artifact manifest`，参数只有 command ID、Project ID 和 occurrence ID；
  没有下载到本地文件的命令或 target path（`src/main.rs:234-244`, `477-490`）。
- 实际运行 `cargo run -- artifact manifest --help` 与顶层/serve help 成功；帮助文本
  明确 metadata query 不写本地文件。
- README 明确进程生命周期、restart 丢失、fixture 未 parse、非 Revision、非持久
  Store，以及磁盘 staging/fsync/replay/orphan cleanup 留待后续
  （`README.md:95-105`）。
- A2 终审要求的两条取消 replay 与 HTTP approval 后最终 snapshot/event 增强断言均
  已落地并通过。

## 确定性门控

在 `/home/mii/code/draft/alda-agent` 实际执行：

```text
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
git diff --check
```

结果全部通过：

- library：17 passed
- CLI binary：3 passed
- real loopback HTTP integration：1 passed
- 合计：21 passed，0 failed
- rustfmt、Clippy `-D warnings`、diff whitespace check：通过

另实际运行顶层 CLI、`artifact manifest` 和 `serve` help，均成功且参数边界与
README 一致。

## 非阻断加固建议

当前 HTTP 负向 Project 用例使用 `project-999`，已经验证无 reachability 时统一
`404`。后续可再增加一个“Project 实际存在，但从未登记目标 hash”的 HTTP 用例，
并与不存在 Project 的响应逐项比较 status/body/cache headers。实现中的精确
`HashSet<(ProjectId, ArtifactHash)>` 检查已经满足语义，因此这只是把 no-oracle
边界锁得更直接，不阻断 A3。

## 最终裁决

A3 实现与两轮设计审查后的冻结契约一致，A1/A2 基线及 A2 后续加固未被破坏。可以
进入 RELEASE；后续 B 不得把本批准误读为对磁盘 durability、crash recovery、正式
Revision/Artifact 引用或 Alda parse 完整性的批准。
