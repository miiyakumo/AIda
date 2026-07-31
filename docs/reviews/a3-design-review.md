---
verdict: revise
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
---

# A3 独立设计审查

## 结论

A3 的范围切分总体清楚：计划明确把磁盘 staging、持久化、Project replay、
orphan 清理及 Revision/Artifact 引用留给 B，并明确 fixture 未经 Alda parse、
不是 Revision、也不是 MIDI/音频。因此，本审查不因 A3 尚未实现磁盘 Store 或
Revision 而否决它。

但当前设计有三项重大问题。它们分别会造成 manifest provenance 无法保持真实、
HTTP 读取绕过单写状态边界，以及 digest/hash 故障时出现“Turn 已成功但 Artifact
不存在”的半提交。修订这些契约后才适合实施。

## 重大问题

### 1. content-addressed blob 去重与单一、不可变 provenance manifest 相互冲突

**位置**

- `docs/plans/mvp-deliberative-execution.md:395-402`
- `docs/plans/mvp-deliberative-execution.md:406-409`
- `docs/design/mvp-design.md:294-298`
- `docs/design/advanced-music-agent-architecture.md:664-673`

**实际证据**

A3 使用固定 fixture bytes，并规定相同 bytes 按 hash 去重；与此同时，
`ArtifactManifest` 包含单一 `project_id`、`session_id`、`turn_id`、created
sequence 和 provenance label，且创建后不可变。计划只说明每个 Project 的可达性
单独登记，没有说明：

1. 不同 Project 产生同一 hash 时，是共用 manifest 还是为 `(project, hash)`
   建立不同 manifest；
2. 同一 Project 的第二个不同 Turn 再次 approve 同一固定 fixture 时，查询
   `artifact.manifest(project_id, hash)` 应返回哪一个 source turn；
3. 第二次 approve 是“生成”新 Artifact、复用旧 Artifact，还是新增一个独立的
   provenance occurrence。

这不是低概率边界：A3 的 fixture bytes 固定，所以同一 Project 连续两个成功 Turn
立即触发该冲突。若 first-writer wins，第二次成功结果中的 source turn 是假的；若
覆盖 manifest，违反不可变；若存两个 manifest，当前只以 `(project_id, hash)`
查询而结果不唯一。高级架构也把 hash/blob 与 metadata event 分开描述，不能据此
推导 manifest 与 blob 是同一个一对一对象。

**影响**

manifest 无法同时满足不可变、可查询、按 hash 去重和来源真实四项要求；测试即使
只覆盖 transport retry，也会遗漏不同命令/不同 Turn 的确定性错误。该模型以后也
无法平滑接入 B 的 Artifact metadata event。

**最小修复方向**

显式拆分：

- `BlobRecord: hash -> immutable bytes/size/mime`，只在这一层按内容去重；
- Project reachability 与 provenance 是独立记录，明确其唯一键和基数。

然后冻结一种无歧义语义。例如为每次成功产生不可变 occurrence/manifest ID，
`(project_id, hash)` 查询返回 occurrence 列表或显式选定的 canonical occurrence；
或者明确“后续成功只复用首次 Artifact，manifest 的 provenance 表示首次创建，
另有不可变 usage/provenance 记录绑定后续 approval”。无论选择哪种，都应增加
“同 Project 不同 Turn 同 bytes”及“跨 Project 同 bytes”验收用例。

### 2. HTTP blob 读取没有可实现的 AppService 边界，直接共享 Store 会绕过单写状态

**位置**

- `docs/plans/mvp-deliberative-execution.md:410-428`
- `docs/design/mvp-design.md:26`
- `docs/design/mvp-design.md:110-117`
- `alda-agent/src/app_service.rs:69-90`
- `alda-agent/src/app_service.rs:148-180`
- `alda-agent/src/http.rs:73-85`
- `alda-agent/src/http.rs:92-101`

**实际证据**

当前唯一状态所有者是 `AppServiceRunner::state`；HTTP state 只持有
`AppService` command sender，所有业务读取和写入都经过同一个 bounded actor。
A3 只新增 wire 级 `artifact.manifest` 命令，却没有定义 HTTP GET 如何取得 blob
bytes、如何在同一个一致性点检查 Project reachability，以及 Store 由谁持有。

若 HTTP router 另持有 `Arc<RwLock<ArtifactStore>>` 并直接读取，它就建立了第二条
业务状态通路：Artifact 创建、Project reachability 与下载校验不再由单一
AppService 串行化。若 router 只持有现有 `AppService`，当前 command reply 又只能
承载 JSON protocol result，没有内部 bytes 响应契约。计划因此不足以指导一个既
可实现又不绕过状态边界的实现。

**影响**

实施者只能自行选择旁路共享状态或临时把二进制塞入公开命令协议。前者破坏统一
AppService/单写边界并可能观察到“blob 已插入但 reachability 尚未登记”的状态；
后者增加无设计依据的 wire 行为和内存复制，也无法自然表达 HTTP headers/304。

**最小修复方向**

冻结一个内部读取契约。最小方案是让同一 actor 接收非 wire 的
`ResolveArtifactDownload { project_id, hash, if_none_match }` 查询，通过 bounded
channel 返回一个不可变下载快照（例如经复验的 manifest + `Arc<[u8]>`，或
NotModified/NotFound/Corrupt）；HTTP adapter 只把该快照映射为响应。创建 blob、
登记 Project reachability 和保存稳定 approval reply 仍必须在同一次 actor
transition 中完成。若采用 writer 发布 immutable snapshot 的方案，也必须明确
发布原子点和 router 只能读取已发布快照，不能直接修改 Store。

### 3. Artifact 校验失败相对于 Approval/Turn 事实的顺序未冻结，可能产生半提交

**位置**

- `docs/plans/mvp-deliberative-execution.md:406-409`
- `docs/plans/mvp-deliberative-execution.md:428`
- `docs/plans/mvp-deliberative-execution.md:432-434`
- `alda-agent/src/app_service.rs:780-812`

**实际证据**

计划要求 digest mismatch 和 hash mismatch 都不生成 Artifact，却只规定成功
`ApprovalDecided` 结果增加 manifest，没有规定 fixture hash/size 校验、blob 插入、
reachability 登记、`ApprovalResolved`、`TurnCompleted(Succeeded)` 和稳定 reply
之间的先后关系及失败状态。

当前 `respond_approval` 在 digest 校验后，先追加 `ApprovalResolved`，再追加
`TurnCompleted(Succeeded)`，最后构造 reply。若实施时沿此结构在末尾生成或复验
Artifact，hash mismatch 会发生在成功事实已经写入之后；若先把 blob 放入 Store
再追加事实，后续失败又可能留下对 Project 可达或不可达但未定义的内存对象。
“mismatch 不生成 Artifact”本身不能保证 Turn/approval 状态不被污染。

**影响**

故障注入下可能得到成功 Turn 却无 manifest/blob，或失败命令留下可下载 blob，
违反 A3 自己的验收标准，也会为 B 引入错误的提交顺序先例。

**最小修复方向**

明确 actor transition 的顺序和失败结果：先在不可见局部值中构造 fixture 并验证
固定 hash/size；任何 mismatch 在追加 approval/terminal facts和登记可达性之前
返回明确内部错误，且 approval 保持 Pending（或明确规定另一种非成功终态）。
验证成功后，在一个不可 `await` 的单写 transition 中同时插入/复用 blob、登记
Project provenance/reachability、追加 approval 与 Turn 事实，并保存包含同一
manifest 的幂等 reply。验收增加故障注入，逐项断言 approval、Turn、store、
reachability 和 reply，而不只断言“没有 Artifact”。

## 重要问题

### 4. 认证 Artifact 的缓存隔离 headers 未定义

**位置**

- `docs/plans/mvp-deliberative-execution.md:419-427`

下载授权同时依赖 session token、Origin 和 `X-Alda-Project-Id`，但响应 header
allowlist 没有 `Cache-Control` 或 `Vary`。URL 只包含 hash；同一 hash 又可跨
Project 存在。客户端或中间缓存若仅按 URL 复用已认证的 `200`，后续请求可能不再
到达服务端执行 Project reachability 检查。虽然 MVP 是 loopback 单用户，这仍会
削弱计划明确要求的跨 Project 读取隔离。

最小修复是冻结认证下载的缓存策略。保守选择为所有成功、304 和错误响应设置
`Cache-Control: private, no-store`；若必须依赖缓存进行自动 ETag revalidation，
则使用 `private, no-cache` 并正确设置覆盖认证、Origin 和 Project header 的
`Vary`。同时明确 `304` 不带 body，ETag 比较发生在认证、hash 解析和 Project
reachability 成功之后，且 corruption 复验不能因 ETag 命中而被跳过，否则
`If-None-Match` 会掩盖损坏内存。

## 已验证为充分或方向正确的部分

- A3 明确声明 `ProcessLifetimeFixture`、未 parse、非 Revision、非持久 Store，
  没有冒充 B。
- wire hash 只接受 `sha256:<64 lowercase hex>`，下载 path 只接受 hex，响应文件名
  仅由服务端 hash 派生；在该契约下没有客户端文件名、绝对路径或 `..` 注入面。
- 不存在与跨 Project 使用相同 not-found，并在查找前认证，方向上可避免 existence
  oracle。
- Deny、取消、invalid choice 和 approval digest mismatch 不创建 Artifact 的范围
  是正确的；需按重大问题 3 补齐原子顺序。
- 固定 MIME、`nosniff`、attachment disposition、长度、quoted ETag、下载时
  hash/size 复验和不支持 Range 的边界均适合 A3；缓存与 304 复验顺序仍需补充。
- transport retry 复用 AppService 已保存的稳定 reply 是可行的，但还必须覆盖
  重大问题 1 所述“不同命令、同一 bytes”的业务语义。

## 建议修订后的最小门控增量

除现有 A3 验收外，至少增加以下确定性用例：

1. 同 Project 两个不同 Turn approve 相同 fixture，manifest/provenance 结果符合
   冻结语义；
2. 两个 Project approve 相同 fixture，blob 去重但 reachability/provenance 隔离；
3. 在 hash/size 校验故障下，approval、Turn、blob、reachability 和幂等 reply 均
   不出现半提交；
4. HTTP 下载只能通过 AppService 的内部只读解析契约取得不可变 bytes；
5. ETag 命中仍先完成认证、Project reachability 和 corruption 检查；
6. 缓存 headers 与跨 Project 相同 hash 的响应隔离符合冻结策略。
