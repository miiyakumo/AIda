---
verdict: revise
scope: final
artifact: /home/mii/code/draft/alda-agent
reviewer: B3a fresh-context independent implementation reviewer
date: 2026-07-31
---

# B3a 独立实现审查

## 结论

B3a 当前不可 RELEASE，verdict 为 `revise`。

实现已闭合设计审查中的多条核心路径：stable reply以 canonical raw JSON bytes +
base64保存并逐字节返回；mutex registry签发单 Project lease；append发生不确定结果后
转换为 consuming poisoned/recover/repair typestate；完整 newline坏行 fail closed；
checkpoint包含完整 command index并从日志 prefix复核 anchor，再 replay tail；checkpoint
损坏回退 full replay。全仓机械门禁也全部通过。

但仍有四项重大问题：stored codec仍直接 Deserialize 多个带构造不变量的 domain值；
poison recovery接受调用者重新提供 Project ID，可把原 Project目录恢复成另一 Project
writer；目录和文件没有执行获批的 private mode/current-owner/regular/有界验证；故障注入
远少于 §15 的强制矩阵，初始化、真实 sync error与 repair barrier均没有确定性证据。

## 重大问题

### M1 — trusted stored codec直接反序列化 domain值，绕过 ID/hash/schema 构造器

- 严重度：重大。
- 位置：
  - `/home/mii/code/draft/alda-agent/src/state_store/project_codec.rs:12-18`
  - `/home/mii/code/draft/alda-agent/src/state_store/project_codec.rs:36-45`
  - `/home/mii/code/draft/alda-agent/src/state_store/project_codec.rs:140-188`
  - `/home/mii/code/draft/alda-agent/src/state_store/project_codec.rs:281-364`
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:11-27`
  - `/home/mii/code/draft/alda-agent/src/domain/mod.rs:99-218`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1118-1132`
- 实际证据：
  - `StoredProjectEventV1` 的 ID、`CreativeBrief`、`Constraint`、`ScoreRevision`、
    `EvidenceEnvelope`与 scope字段直接使用 domain类型并 derive `Deserialize`。
  - domain ID newtype和 `ArtifactHash`也直接 derive `Deserialize`；serde会写入私有
    tuple字段，不调用 `DomainProjectId::parse`、其他 ID parse或 `ArtifactHash::parse`。
  - codec对 Brief/Constraint/Revision/Evidence的 `into_domain`只是移动已反序列化值；
    只有 Human/Waiver/Artifact audit使用显式 trusted constructor。
  - reducer会验证部分关系和文本，但 `ProjectInitialized`直接接受 score/take/branch ID，
    也不会重新验证所有 nested ID、`SchemaVersion != 0`或 hash canonical form。
- 影响：
  - 一个 checksum正确但包含空值、路径分隔符、控制字符、零 schema或非 canonical hash的
    stored batch可进入 domain projection；这违反 §15 “conversion重新调用
    ID/hash/scope/audit/decision构造器”的可信边界。
  - B3a由此重新引入了 B1最终裁决明确要求隔离的“serde即可信事实”路径。后续 checkpoint
    会把非法状态重新序列化并固化。
- 最小修复方向：
  - stored DTO全程使用原始 primitive/stored enum；conversion逐字段调用所有 domain
    parse/constructor/validate函数，不能在 serde层直接构造带不变量的 domain值。
  - 增加 checksum重算后的 invalid ID、invalid hash、zero schema、invalid nested scope/
    evidence/revision fixture，确认 conversion在 reducer前或 reducer中 fail closed。

### M2 — poisoned recovery的 Project identity由调用者重新注入，可污染原 lease目录

- 严重度：重大。
- 位置：
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:631-653`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:656-700`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1113-1161`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1525-1564`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1143-1152`
- 实际证据：
  - `PoisonedProjectWriter`只保存 lease与 Project directory fd，不保存原
    `DomainProjectId`。
  - `recover(self, project_id)`公开要求调用者再次传入任意 Project ID，并将它作为
    `scan_log` expected identity及 `RepairRequiredWriter.project_id`。
  - 初次 append发生 partial write时没有任何完整 batch可与 expected Project ID比对。
    此时传入 Project B会得到 B的 empty recovered state；repair截断后构造持有原 Project
    A registry key/directory fd、但内存 `state.project_id = B`的 Ready writer。
  - 后续 append会把 Project B batch写入以 Project A canonical hash命名的目录。现有测试
    只以原 Project ID调用 recover，没有覆盖 identity substitution。
- 影响：
  - Project目录名、writer lease key、batch Project ID三者可分裂；重开原 Project A将
    `StreamMismatch`，而打开 Project B会查看另一个目录，造成权威日志不可达/拒绝服务。
  - 这破坏 Project hash collision/mismatch fail-closed与 consuming lease identity不变式。
- 最小修复方向：
  - Ready/Poisoned/RepairRequired typestate均保存同一个原始 Project ID；`recover(self)`
    不接受调用者参数，只移动内部 identity。
  - recovery与repair前验证 lease key等于该 Project canonical hash；增加初批 partial
    write后尝试替换 Project ID的负向测试。

### M3 — fd-relative路径存在，但受管对象的权限、owner、类型和读取上限未达到安全契约

- 严重度：重大。
- 位置：
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1208-1236`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1239-1267`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1270-1329`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:935-960`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1068-1082`
- 实际证据：
  - `validate_directory`对 root只拒绝 group/world **write** bits，而设计要求 private；
    对 layout/projects/Project目录完全不检查 mode，只检查 directory与当前 UID。
  - events file只检查 `metadata().is_file()`，不检查当前 UID或 group/world mode；
    `open_events`甚至不检查 regular file/owner/mode。
  - checkpoint只检查 `is_file`和 1 MiB size，不检查 owner/private mode。
  - manifest打开时没有 `NONBLOCK`、same-handle regular/UID/mode/size检查，并使用
    `read_to_end`无上限读取。`O_NOFOLLOW`不能拒绝 FIFO；同 UID预置 FIFO会在 open/read
    阻塞，超大 regular manifest可造成无界分配。
  - 文件创建mode受 umask变窄可以接受，但 reopening现有对象时没有证明其满足0600/private。
- 影响：
  - Store可在设计明确判为 unsafe的共享可读目录/文件上签发 durability capability；
    同 UID异常对象还能阻塞初始化或扩大内存，和 B2已修复的 pin special-file问题同类。
  - trusted log包含 Human决定、Artifact reachability及stable replies；弱权限会扩大
    本地泄露/篡改面，并使“0700/current-uid root是checksum信任边界”的前提不成立。
- 最小修复方向：
  - 对root及所有受管目录从同一 fd验证current UID和无group/world权限；对manifest、
    events、checkpoint验证regular/current UID/private mode及明确size上限。
  - 特殊文件读取使用nonblocking/no-follow打开并在读取前验证metadata；manifest采用固定
    小上限读取。增加 FIFO/directory/symlink/wrong-owner/weak-mode/oversized fixtures。

### M4 — failpoint与测试矩阵没有覆盖获批的初始化、sync error及repair durability路径

- 严重度：重大。
- 位置：
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:374-396`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:464-510`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:580-628`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:665-700`
  - `/home/mii/code/draft/alda-agent/src/state_store/mod.rs:1466-1721`
  - `/home/mii/code/draft/docs/plans/mvp-deliberative-execution.md:1182-1203`
- 实际证据：
  - append只有 `BeforeWrite`、`PartialWrite`、`AfterNewlineBeforeSync`、
    `AfterSyncBeforeUpdate`；没有让 `flush/sync_all`本身返回错误的 failpoint。
  - checkpoint只有 temp write、sync前、install前、dir sync前四点；没有 temp create、
    install后/dir sync error状态或残留清理验证。
  - manifest、layout/projects/Project dir、events create以及各级 fsync完全没有
    failpoint；repair rescan race、truncate、file sync、dir sync也没有任何注入点。
  - §15明确要求上述初始化与repair点，并要求所有 failpoint；现有9个 B3a测试只覆盖
    happy path、两种 append阶段、单一完整坏行和checkpoint四个逻辑阶段。
- 影响：
  - B3a的产品承诺是“fsync后的 batch才提交”及“repair compare→truncate→file sync→dir
    sync”；缺少 barrier error测试，无法证明错误时不会返回 Ready、释放lease后错误续写、
    覆盖权威 checkpoint/manifest或隐藏未同步状态。
  - happy-path门禁不能替代失败原语的确定性证据，尤其 M2显示 typestate identity仍可在
    未覆盖分支中破坏。
- 最小修复方向：
  - 将state filesystem原语收口到可注入backend/完整逻辑gate，覆盖获批清单中的每个
    create/write/flush/sync/install/truncate/rescan race/dir sync阶段。
  - 每点断言：不返回成功或错误Ready、不破坏已提交prefix、lease状态明确、reopen结果
    唯一；repair失败保持Corrupt/不可append，checkpoint失败仅丢缓存。

## 已验证通过

- `StoredCommandRecordV1`限制raw reply为64 KiB，验证 UTF-8/versioned `CommandReply`、
  client command ID与canonical re-encode，standard padded base64解码后返回原始bytes。
- command index key为 `(client_id, client_command_id)`；同digest返回旧bytes且不append，
  不同digest返回`IdempotencyConflict`。
- mutex registry原子insert Project key，lease不可Clone且Drop释放；同一Store内相同
  Project第二writer拒绝，不同Project可并存。
- write开始后的partial/newline/sync后逻辑故障产生Poisoned writer；recover消费lease，
  RepairRequired/Corrupt不能append，repair复扫并比较valid bytes、damaged bytes与tail
  digest后才truncate/sync。
- scan对无newline最后片段返回recoverable tail；任何newline完整但JSON无效的line返回
  committed-area corruption，不跳过后续记录。
- batch校验schema、Project、stream/epoch、sequence、transaction唯一性、checksum chain、
  command唯一性，并通过B1 reducer重建projection。
- checkpoint保存events、完整command index、transaction IDs、covered byte offset/
  sequence/checksum、projection/digest；load从offset 0复放prefix验证anchor和index，
  再从covered offset replay tail。损坏checkpoint回退full log。
- root从 `/` fd逐组件`openat(O_DIRECTORY|O_NOFOLLOW)`，root以下目录和文件操作使用
  descriptor-relative API；新目录同步child inode及parent entry，batch成功前
  `sync_all(events)`。
- B3a未接生产 App Service写面，也未把Project与Session事件混入同一stream。

## 机械门禁

在 `/home/mii/code/draft/alda-agent` 实际运行：

```text
cargo fmt --check                                         PASS
cargo clippy --all-targets --all-features -- -D warnings PASS
cargo test --all-targets --all-features                  PASS
  lib unit tests                                         57 passed
  main unit tests                                         6 passed
  HTTP integration                                        1 passed
  WS integration                                          2 passed
node --test web/client-state.test.js                     PASS (5 passed)
node --check web/app.js                                  PASS
node --check web/client-state.js                         PASS
node --check web/sw.js                                   PASS
git diff --check                                         PASS
```

机械门禁通过不覆盖上述stored constructor绕过、cross-Project recovery、弱权限/special file
及缺失failure matrix，因此 verdict仍为`revise`。

## RELEASE判定

- Slice A / B1 / B2回归：PASS。
- B3a主事务与checkpoint happy path：主体正确。
- B3a：**不可 RELEASE**。
- B3b/B4与正式MVP：本报告不作完成声明。
