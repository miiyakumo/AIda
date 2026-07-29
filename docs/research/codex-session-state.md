# codex 调研:会话持久化、resume 与上下文压缩

> 调研对象:`ref/codex/codex-rs/`(下文所有相对路径均以此为根)。校验基准: commit `61a44880a85d2fd0d8770908dea5733495e571c8`。
> 目的:为 mini alda harness 的"最简持久化 + 压缩"设计提供依据。所有路径、字段、行号均经过源码核对。
>
> 一句话总览:codex 把**会话的权威记录**放在一个 append-only 的 JSONL 文件(rollout)里,SQLite 只是可丢弃的元数据索引;resume 就是"读回 JSONL → 逆序找最近的压缩检查点 → 正序重放尾部";压缩的触发依据是 **API 返回的 token 用量**超过 `auto_compact_token_limit`(默认 = context window 的 90%),做法是让模型自己生成一段 handoff 摘要,然后用「保留的用户消息 + 摘要」**整体替换**历史。

---

## 1. 全景:会话状态的分层

codex 的会话状态分四层,职责严格分离:

| 层 | crate / 目录 | 内容 | 可否丢弃 |
|---|---|---|---|
| rollout(权威) | `rollout/`(crate `codex-rollout`) | 每会话一个 JSONL 文件,记录所有需要持久化的事件 | 不可,唯一事实来源 |
| 内存历史 | `core/src/context_manager/` | `ContextManager`:发给模型的 `Vec<ResponseItem>` + token 统计 | 进程退出即消失,靠 rollout 重建 |
| SQLite 索引 | `state/`(crate `codex-state`) | 从 rollout 提取的线程元数据镜像,用于列表/搜索 | 可丢弃,坏了回退到文件系统扫描 |
| 全局消息历史 | `message-history/` | `~/.codex/history.jsonl`,跨会话的用户输入历史(供 TUI 上翻) | 可丢弃 |

- SQLite 层的定位见 `state/src/lib.rs:1-5`:"extracts rollout metadata from JSONL rollouts and mirrors it into a local SQLite database"——它只是镜像。列表线程时的"文件系统兜底 + 读修复"逻辑在 `rollout/src/recorder.rs:437-702`(`list_threads_with_db_fallback`):DB 不可用或出错时直接扫文件系统返回,并调用 `state_db::reconcile_rollout` 修复过期行。
- 全局消息历史的格式见 `message-history/src/lib.rs:1-15`:每行 `{"session_id":"<uuid>","ts":<unix_seconds>,"text":"<message>"}`,用 `O_APPEND` + 单次 `write(2)` 保证并发追加的原子性,文件名常量 `HISTORY_FILENAME = "history.jsonl"`(`message-history/src/lib.rs:52`)。
- 会话与线程的存储接口被抽象为 `ThreadStore` trait(`thread-store/src/lib.rs:1-5`:"Application code should treat ThreadId as the only durable thread handle"),本地实现 `LocalThreadStore` 底下就是 rollout + state db。

**对 mini harness 的启示**:只需要第 1、2 层。索引和全局历史都是规模大了以后的优化。

---

## 2. 会话以什么格式落盘(rollout JSONL)

### 2.1 文件位置与命名

`rollout/src/recorder.rs:1549-1578`(`precompute_log_file_info`):

```
~/.codex/sessions/YYYY/MM/DD/rollout-YYYY-MM-DDThh-mm-ss-<ThreadId>.jsonl
```

- `SESSIONS_SUBDIR = "sessions"`、归档目录 `ARCHIVED_SESSIONS_SUBDIR = "archived_sessions"`(`rollout/src/lib.rs:25-26`);
- 文件名时间用 `-` 代替 `:`,注释明确写了是为了兼容不允许冒号的文件系统(`recorder.rs:1563-1565`);
- 文件名里内嵌 ThreadId(UUID),所以**仅凭文件名就能拿到创建时间和会话 id**,列表分页的 cursor 就是靠解析文件名实现的(`recorder.rs:1141` 调用 `parse_timestamp_uuid_from_filename`)。

### 2.2 行格式:RolloutLine

每行一个 JSON 对象,类型定义在 `protocol/src/protocol.rs:3379-3386`:

```rust
pub struct RolloutLine {
    pub timestamp: String,            // "2025-05-07T17:24:21.123Z"
    pub ordinal: Option<u64>,         // 仅 Paginated 模式有,单调递增
    #[serde(flatten)]
    pub item: RolloutItem,
}
```

`RolloutItem` 是 tagged enum(`protocol.rs:3184-3199`,`#[serde(tag = "type", content = "payload", rename_all = "snake_case")]`),所以每行长这样:

```jsonc
{"timestamp":"...","type":"response_item","payload":{ ...ResponseItem... }}
{"timestamp":"...","type":"event_msg","payload":{ ...EventMsg... }}
```

七种 item:

| type | 内容 | 作用 |
|---|---|---|
| `session_meta` | `SessionMetaLine`(`protocol.rs:3147-3153`,SessionMeta + git 信息) | **必须是第一行**;会话身份 |
| `response_item` | `ResponseItem`(消息/推理/工具调用/工具输出…) | 模型可见历史的原料 |
| `compacted` | `CompactedItem`(`protocol.rs:3220-3236`) | 压缩检查点:摘要 + **完整替换历史** |
| `turn_context` | `TurnContextItem`(`protocol.rs:3262-3306`,cwd/model/approval_policy/sandbox…) | 每个用户 turn 记一次,resume 时恢复运行配置 |
| `world_state` | `WorldStateItem`(`protocol.rs:3202-3207`,full 快照或 merge patch) | 模型可见环境状态的 diff 基线 |
| `event_msg` | `EventMsg` 白名单子集 | turn 边界、token 计数等标记 |
| `inter_agent_communication`(及 metadata) | 多 agent 通信 | mini harness 不需要 |

`SessionMeta`(`protocol.rs:3057-3114`)的核心字段:`session_id`、`id`(ThreadId)、`timestamp`、`cwd`、`originator`、`cli_version`、`model_provider`、`base_instructions`、`history_mode`(`Legacy`/`Paginated`,`protocol.rs:691-695`,默认 Legacy)、fork/父线程信息等。首行由 `write_session_meta` 写入并顺带采集 git commit/branch/URL(`recorder.rs:1806-1833`)。

### 2.3 持久化策略:哪些写、哪些不写

过滤逻辑集中在 `rollout/src/policy.rs`,在 thread-store 写入前统一应用(`thread-store/src/local/live_writer.rs:301` 调用 `persisted_rollout_items`):

- `is_persisted_rollout_item`(`policy.rs:9-21`):`SessionMeta`/`Compacted`/`TurnContext`/`WorldState` 一律持久化;`ResponseItem` 和 `EventMsg` 各走白名单。
- `should_persist_response_item`(`policy.rs:39-59`):消息、推理、各类工具调用与输出、压缩 item 都存;`AdditionalTools`、`Other` 不存。
- `should_persist_event_msg`(`policy.rs:87-183`):**绝大多数 UI 事件不落盘**。永久保留的只有:`TokenCount`、`TurnStarted`、`TurnComplete`、`TurnAborted`、`ThreadRolledBack`、`ThreadGoalUpdated`、`ThreadSettingsApplied`;`UserMessage`/`AgentMessage` 等 legacy 事件仅在 `history_mode == Legacy` 时保留。流式 delta、审批请求、exec begin/end 等全部丢弃。

**要点**:落盘的是"重建模型上下文所需的最小集合 + 少量分析标记",不是 UI 事件流水账。

### 2.4 写入机制(RolloutRecorder)

`rollout/src/recorder.rs`:

- `RolloutRecorder`(`recorder.rs:84-89`)持有一个 `mpsc::channel::<RolloutCmd>(256)`(`recorder.rs:892`),文件句柄由一个后台 tokio task 独占(`rollout_writer`,`recorder.rs:1774-1804`),命令有 `AddItems` / `Persist` / `Flush` / `Shutdown`(`recorder.rs:116-128`)。
- **延迟物化**:新会话创建时只预计算路径,不建文件;首次 `persist()` 才真正 open+写 SessionMeta(`recorder.rs:787-792` 注释、`RolloutWriterState::is_deferred`,`recorder.rs:1678-1680`)。避免空会话留下垃圾文件。
- **失败重试**:待写 items 先进 `pending_items`,写成功才逐条移除;I/O 失败丢弃文件句柄、保留未写后缀,下个 barrier 重开文件重试(`RolloutWriterState` 文档注释,`recorder.rs:1603-1617`;`write_pending_with_recovery`,`recorder.rs:1651-1676`)。
- 每行写完立即 `flush`(`JsonlWriter::write_line`,`recorder.rs:1922-1928`);append 前保证文件以 `\n` 结尾(`ensure_rollout_is_newline_terminated`,`recorder.rs:1874-1887`)——防止上次崩溃留下半行导致两条记录粘连。
- Paginated 模式给每行发 `ordinal`(`rollout/src/ordinal.rs:17-53`);resume 追加时从文件尾反向扫描最后一条合法记录的 ordinal 续号(`ordinal_state_for_rollout`,`ordinal.rs:56-101`)。

### 2.5 冷文件压缩(zstd)

`rollout/src/compression.rs`:后台 worker 把冷的 `.jsonl` 压成 `.jsonl.zst`(`COMPRESSED_SUFFIX = ".zst"`,`compression.rs:18`;`spawn_rollout_compression_worker`,`compression.rs:29-31`);读取端 `open_rollout_line_reader`(`compression.rs:47-58`)对两种表示透明;需要追加时先解压物化回 plain(`materialize_rollout_for_append_blocking`,`compression.rs:77+`)。这是纯磁盘空间优化,与"上下文压缩"无关。

---

## 3. resume:如何重建状态

### 3.1 读回文件

`RolloutRecorder::load_rollout_items`(`recorder.rs:982-1045`):

- 逐行 `serde_json` 解析,**坏行只计数、告警、跳过**,不让单行损坏毁掉整个会话(`recorder.rs:996-1022`);
- 以文件中**第一条** `session_meta` 的 `id` 为权威 thread_id(`recorder.rs:1026-1032`,后续的 session_meta 可能是 fork 复制来的);
- 全空文件报错 "empty session file"。

`get_rollout_history`(`recorder.rs:1047-1062`)把结果包成 `InitialHistory::Resumed(ResumedHistory { conversation_id, history, rollout_path })`(类型在 `protocol.rs:2541-2556`;`InitialHistory` 共四个变体:`New`/`Cleared`/`Forked`/`Resumed`)。

### 3.2 会话启动时应用历史

`core/src/session/mod.rs:1286-1401`(`record_initial_history`),`Resumed` 分支(`mod.rs:1313-1352`)做四件事:

1. `apply_rollout_reconstruction`(`mod.rs:1411-1467`)重建内存历史(见 3.3);
2. 若上次的 model 与本次不同,发 Warning 提醒用户(`mod.rs:1320-1338`);
3. **恢复 token 计数**:从 rollout 里逆序找最后一条 `EventMsg::TokenCount` 的 `TokenUsageInfo` 直接装回(`last_token_info_from_rollout`,`mod.rs:1485-1490`)——这就是"resume 后 UI 立即能显示用量"的全部秘密;
4. 继续以 append 模式打开同一文件接着写(`RolloutRecorderParams::Resume`,`recorder.rs:873-885`)。

### 3.3 历史重建算法(两遍扫描)

`core/src/session/rollout_reconstruction.rs:113-440`(`reconstruct_history_from_rollout`),这是整个 resume 的核心,设计成 **逆序扫描 + 正序重放**:

**第一遍:从新到旧逆序扫**(`rollout_reconstruction.rs:154-295`),目的:

- 找到**最新的存活压缩检查点**:第一个带 `replacement_history` 的 `Compacted` item(`rollout_reconstruction.rs:181-186`)。找到它意味着更老的记录全部无关——`replacement_history` 是压缩时的完整替换历史,自身就是一个合法的历史基线;
- 顺便收集 resume 元数据:最新存活 user turn 的 `TurnContextItem`(恢复 model/comp_hash 等 `previous_turn_settings`)、上下文窗口链(window_number / window_id)、world-state 重放序列;
- 处理回滚:`ThreadRolledBack(n)` 在逆序里变成"跳过接下来 n 个 user-turn 段"(`rollout_reconstruction.rs:70-78, 188-191`);
- 三样元数据齐了就提前 break(`rollout_reconstruction.rs:286-294`)。

**第二遍:对检查点之后的尾部正序重放**(`rollout_reconstruction.rs:317-373`):

- 先 `history.replace(replacement_history)` 装入基线(`rollout_reconstruction.rs:319-321`);
- `ResponseItem` → `history.record_items(...)`(带工具输出截断,与实时路径同一套逻辑);
- 尾部再遇到 `Compacted`:有 `replacement_history` 直接 replace;没有(legacy)则用 `collect_user_messages` + `build_compacted_history` 现场重建(`rollout_reconstruction.rs:341-363`);
- `ThreadRolledBack` → `history.drop_last_n_user_turns(n)`;
- world-state 记录按时间序重放:`Compacted` 清基线,full 快照重建、patch 合并(`rollout_reconstruction.rs:389-422`)。

**要点**:JSONL 里存的是"事件日志",内存历史是"事件日志的投影";压缩检查点(`replacement_history`)相当于日志里内嵌的 snapshot,让重放不必从头开始。这正是经典的 event sourcing + snapshot 模式。

### 3.4 内存历史:ContextManager

`core/src/context_manager/history.rs`:

- `items: Arc<Vec<ResponseItem>>`(`history.rs:40-60`),快照共享、写时复制;
- `record_items`(`history.rs:124-138`):过滤非 API 消息(system 角色、`Other` 等,`is_api_message`,`history.rs:459-479`),并在**入册时**就按 `TruncationPolicy × 1.2` 截断工具输出(`process_item`,`history.rs:344-387`)——大输出从不原样进入历史;
- 发请求前 `for_prompt`(`history.rs:143-146`)做归一化(`normalize_history`,`history.rs:328-342`):补齐无输出的 call、删孤儿 output、剥离模型不支持的图像/音频;
- `replace`(`history.rs:203-207`)整体换掉历史并 `history_version += 1`(压缩、回滚都走这里)。

---

## 4. token 用量:如何统计与限制

### 4.1 数据结构

`protocol/src/protocol.rs:2056-2137`:

```rust
pub struct TokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}
pub struct TokenUsageInfo {
    pub total_token_usage: TokenUsage,   // 会话累计(跨请求求和)
    pub last_token_usage: TokenUsage,    // 最近一次请求(≈当前上下文占用)
    pub model_context_window: Option<i64>,
}
```

关键区分:**`last_token_usage.total_tokens` 才是"当前上下文有多满"的依据**(上一次请求的完整 prompt+output 大小);`total_token_usage` 是累计计费口径。

### 4.2 统计路径

1. 模型流结束时,`ResponseEvent::Completed { token_usage, .. }` 带回 API 的 usage(如 `compact.rs:726-741` 的 `drain_to_completed`);
2. `Session::update_token_usage_info`(`core/src/session/mod.rs:3776-3786`)→ `record_token_usage_info` 更新 `TokenUsageInfo` → `send_token_count_event`(`mod.rs:3897-3904`)发出 `EventMsg::TokenCount`;
3. `TokenCount` 在持久化白名单里(`policy.rs:98`),于是每次请求后的用量都写进了 rollout —— resume 恢复用量(3.2 第 3 点)因此成立。

### 4.3 "当前占用"的混合算法

API 只在请求结束时报数,请求之间本地新增的 items(工具输出等)没被计入。`ContextManager::get_total_token_usage`(`history.rs:297-315`)用混合口径:

```
当前占用 = last_token_usage.total_tokens          // API 实报
         + Σ estimate(最后一个模型生成 item 之后的本地 items)   // 本地估算补差
         + (若服务端未计推理) Σ estimate(历史中的加密 reasoning)
```

估算器 `estimate_item_token_count`(`history.rs:497-500`):JSON 序列化字节数 ÷ 4(`approx_tokens_from_byte_count_i64`);图像统一按 7373 字节估(`RESIZED_IMAGE_BYTES_ESTIMATE`,`history.rs:506`);加密 reasoning 按 base64 解码后长度 ×3/4−650 估(`history.rs:481-487`)。注释明确说这是 "coarse lower bound, not a tokenizer-accurate count"(`history.rs:162-163`)。

### 4.4 限制与阈值

- 模型元数据里有 `context_window` 与可选的 `auto_compact_token_limit`(`protocol/src/openai_models.rs:408-417`);
- **默认压缩阈值 = context_window 的 90%**,配置值也会被钳到 90% 以内:`ModelInfo::auto_compact_token_limit()`(`openai_models.rs:459-470`,`(context_window * 9) / 10`);
- 每次采样后计算 `ContextWindowTokenStatus`(`core/src/session/context_window.rs:23-91`):`token_limit_reached = 用量 ≥ auto_compact_limit(+可选缓冲) || 用量 ≥ context_window`(`context_window.rs:74-79`);
- UI 百分比:`percent_of_context_window_remaining`(`protocol.rs:2247-2258`),分子分母都先减去 `BASELINE_TOKENS = 12000`(`protocol.rs:2213`,系统提示+工具+压缩余量的固定开销),让"100%"对应用户真正可支配的窗口;
- 另有两个独立预算机制:会话级花费预算(超了直接 `SessionBudgetExceeded` 报错,`core/src/session/rollout_budget.rs:26-36`)和 TokenBudget 提醒(剩余量低于阈值时向历史注入提醒消息,`core/src/session/token_budget.rs:6-61`)。

---

## 5. 上下文压缩(compaction)

### 5.1 什么条件触发

四个触发点,全部收敛到同一个 `run_auto_compact`:

1. **采样后(mid-turn)**:`core/src/session/turn.rs:336-423`。每次模型请求结束,检查 `token_limit_reached`;若模型还要继续(needs_follow_up)且超限,`should_roll_over = true`(`turn.rs:383-384`),立刻做 mid-turn 压缩再继续本 turn(`turn.rs:395-423`);
2. **采样前(pre-turn)**:`run_pre_sampling_compact`(`turn.rs:845-875`)。新 turn 开始前先查一次,超限就先压缩;
3. **模型切换**:`maybe_run_previous_model_inline_compact`(`turn.rs:914-1008`)。`comp_hash`(压缩兼容性哈希)变化,或切到更小 context window 的模型且旧用量装不下,先用**旧模型**做一次压缩;
4. **手动 `/compact`**:`run_compact_task`(`core/src/compact.rs:143-167`)。

### 5.2 三种实现的选择

`run_auto_compact`(`turn.rs:1012-1089`)按序判断:

- `Feature::TokenBudget` 开启 → **token-budget 压缩**:不做任何总结,直接 `start_new_context_window` 开新窗口(`core/src/compact_token_budget.rs:21-93`);
- provider 支持远程压缩 → **remote 压缩**:把整个历史发给 Responses API 的 compaction 请求(`CodexResponsesRequestKind::Compaction`),由服务端返回(加密的)压缩结果替换历史;发送前先把超窗的工具输出改写为 "Output exceeded the available model context and was truncated"(`core/src/compact_remote.rs:47-48`、`compact_remote_request.rs:23-63`);
- 否则 → **local 压缩**(自建 harness 最值得抄的路径,见 5.3)。

### 5.3 local 压缩的具体做法

`core/src/compact.rs:240-393`(`run_compact_task_inner_impl`):

**第一步:让模型自己写 handoff 摘要。** 把压缩指令作为一条普通 user 消息 append 到当前历史(`compact.rs:250-256`),发一次普通请求。指令全文(`SUMMARIZATION_PROMPT`,`prompts/templates/compact/prompt.md`,经 `prompts/src/compact.rs:1` include):

> You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another LLM that will resume the task.
>
> Include:
> - Current progress and key decisions made
> - Important context, constraints, or user preferences
> - What remains to be done (clear next steps)
> - Any critical data, examples, or references needed to continue
>
> Be concise, structured, and focused on helping the next LLM seamlessly continue the work.

**容错**:压缩请求本身又超窗时,从历史头部逐条删 item 重试(保留前缀缓存与最近消息,`compact.rs:309-324` 调 `history.remove_first_item()`);普通网络错误按 backoff 重试。

**第二步:构造替换历史。**(`compact.rs:347-365`)

- 取模型回复的最后一条 assistant 消息作摘要,拼上 `SUMMARY_PREFIX`(`prompts/templates/compact/summary_prefix.md`):"Another language model started to solve this problem and produced a summary of its thinking process. … use the information in this summary to assist with your own analysis";
- `collect_user_messages`(`compact.rs:525-548`)抽出历史里所有**真实用户消息**(剔除旧摘要);
- `build_compacted_history`(`compact.rs:611-685`):从**最新往旧**挑用户消息,总预算 `COMPACT_USER_MESSAGE_MAX_TOKENS = 20_000`(`compact.rs:56`),超出的最旧一条截断;新历史 = `[保留的用户消息…, "SUMMARY_PREFIX\n摘要"(user 角色)]`。

**保留 vs 丢弃**:

| 保留 | 丢弃 |
|---|---|
| 真实用户消息(新→旧凑 ≤20k tokens) | 所有 assistant 回复(浓缩进摘要) |
| 模型生成的摘要(user 角色注入) | 所有 reasoning |
| (mid-turn)重新注入的初始上下文 | 所有工具调用与工具输出 |
| | 旧的压缩摘要 |

**摘要位置规则**(`InitialContextInjection`,`compact.rs:58-73`):pre-turn/手动压缩不注入初始上下文(下一 turn 自然全量重注入);mid-turn 压缩因为"模型被训练成期望摘要是历史最后一项",初始上下文插到最后一条真实 user 消息**之前**(`insert_initial_context_before_last_real_user_or_summary`,`compact.rs:564-609`)。

**第三步:替换 + 落盘。** `Session::replace_compacted_history`(`core/src/session/mod.rs:3188-3234`):

- 内存里 `state.replace_history(items, ...)`;
- 向 rollout 写一条 `Compacted { message, replacement_history: Some(items), window_number, first/previous/window_id }`——**替换历史全文随检查点落盘**,这就是 3.3 中 resume 能走捷径的原因;
- 随后补写 `WorldState`(full 快照)与 `TurnContext`;
- 压缩后 `recompute_token_usage`(`mod.rs:3824-3861`)用本地估算值重置 `token_info`(此时没有 API 实报数);
- 每次压缩推进"上下文窗口链":`window_number+1`、UUIDv7 的 window_id 链(`advance_auto_compact_window`,`mod.rs:3630-3633`;窗口标识 `"{thread_id}:{window_number}"`,`mod.rs:3623-3628`);
- 最后给用户发 Warning:"Long threads and multiple compactions can cause the model to be less accurate. Start a new thread when possible…"(`compact.rs:388-391`)。

---

## 6. mini harness(alda agent)的最简持久化与压缩方案

以下按"抄 codex 的骨架、砍掉规模化机制"的原则给出建议,并标注每项抄自哪里。

### 6.1 JSONL 会话日志

一个会话一个文件:`sessions/2026-07-27T14-30-00-<uuid>.jsonl`(抄 codex 的"文件名内嵌时间+id",省去任何索引)。每行:

```jsonc
{"ts":"2026-07-27T14:30:00.123Z","type":"...","payload":{...}}
```

最简 item 集合(codex 七种砍到五种):

| type | payload | 对应 codex |
|---|---|---|
| `session_meta` | `{id, created_at, cwd, provider, model, base_instructions}` | SessionMeta(首行,必须) |
| `response_item` | 统一内部消息类型(user/assistant/tool_call/tool_output) | ResponseItem |
| `token_count` | `{input, cached_input, output, total}` | EventMsg::TokenCount |
| `compacted` | `{summary, replacement_history: [...]}` | CompactedItem |
| `score_state` | `{path, alda_source}` — 当前乐谱全文快照 | WorldStateItem(见 6.4) |

写入策略:单用户 CLI 直接**同步 append + flush,每行一个完整 JSON + `\n`** 即可,不需要 codex 的后台 writer/mpsc/重试机制(那些是为多前端并发与不可中断 UI 服务的)。但两条细节值得抄:
- append 前检查文件尾是否 `\n`(防崩溃半行,`recorder.rs:1874-1887`);
- 读回时坏行跳过并计数,不让单行损坏毁掉会话(`recorder.rs:996-1022`)。

落盘过滤:只写上表五种;流式 delta、播放事件、人耳评价的 UI 交互一概不写(人评价以 user 消息身份进 `response_item`,自然被持久化)。

### 6.2 resume

M1 可以比 codex 简单一个数量级,但骨架相同:

1. 逐行读全文件,取第一条 `session_meta`;
2. **逆序找最后一条 `compacted`**:有 → 以其 `replacement_history` 为基线,只正序重放其后的 `response_item`;无 → 从头重放全部 `response_item`;
3. 逆序找最后一条 `token_count` 恢复用量显示;逆序找最后一条 `score_state` 恢复乐谱;
4. 以 append 模式继续写同一文件。

不需要:ordinal/Paginated、fork、rollback、world-state merge-patch、SQLite、zstd。什么时候才需要:会话数上千(索引)、要"从历史某点分叉重试"(fork)、要撤销(rollback)。

### 6.3 token 统计与压缩触发

- 内部统一一个 `TokenUsage` 结构(抄 `protocol.rs:2056-2070`,砍掉 reasoning 字段),两个 provider 的 usage 都映射过来:Anthropic Messages API 的 `usage.input_tokens/output_tokens/cache_read_input_tokens/cache_creation_input_tokens`;OpenAI Responses API 的 `usage.input_tokens/output_tokens/total_tokens`。**当前占用 = 最近一次请求的 input+output 总量**(codex 的 `last_token_usage.total_tokens` 口径),不要用累计值判断压缩;
- 请求之间本地新增的工具输出用 `bytes/4` 粗估补差(抄 `history.rs:297-315` 的混合口径);
- 触发条件:`当前占用 ≥ 0.8 × context_window` 时,在**下一 turn 开始前**压缩(即只做 codex 的 pre-turn 路径,`turn.rs:845-875`;mid-turn 压缩要处理"压缩后接着跑半个 turn"的状态,复杂度不值得)。0.8 比 codex 的 0.9 保守,给"压缩请求本身也要占窗口"留余量;
- 工具输出在**入册时**就截断(抄 `history.rs:344-387`):alda parse 错误信息通常很短,但 `alda parse -o data` 的 score JSON 可能很大——设每条工具输出上限(如 8k tokens),超出时保留头尾。反正 score JSON 随时可以对着乐谱文件重新生成,信息丢失是可恢复的。

### 6.4 压缩做法(alda 特化)

抄 local 压缩的两段式,但利用领域特性:

1. **乐谱即世界状态**:alda agent 的核心状态是乐谱文件本身,它是紧凑的符号文本。压缩时把"当前乐谱全文 + 文件路径"作为 `score_state` 行落盘,并注入替换历史(对应 codex 的 WorldState full 快照,`mod.rs:3207-3225`)。这比让模型在摘要里复述乐谱可靠得多;
2. 压缩指令抄 `prompts/templates/compact/prompt.md` 的四要点结构,替换为音乐语境(已做的创作决定:调性/曲式/配器;用户的偏好反馈;还剩什么没写;当前 parse 是否通过);
3. 替换历史 = `[保留的用户消息(含人耳反馈,≤ 若干 k tokens), score_state 注入, "摘要前缀\n摘要"]`;丢弃全部 assistant 回复、工具调用与输出——旧的 parse 错误对新窗口毫无价值;
4. 写一条 `compacted`(带 `replacement_history` 全文),内存历史整体 `replace`。**务必把替换历史全文随检查点落盘**——这是 codex resume 算法能"逆序找到检查点就停"的前提(`rollout_reconstruction.rs:181-186`),也让你的 resume 实现保持 20 行以内。

### 6.5 骨架代码量预估

| 模块 | 对应 codex | 预估 |
|---|---|---|
| `session_log.rs`(append/读回/容错) | rollout/recorder + policy | ~150 行 |
| `history.rs`(Vec\<Item\> + 截断 + 估算) | context_manager/history | ~120 行 |
| `resume.rs`(检查点 + 重放) | rollout_reconstruction | ~60 行 |
| `compact.rs`(触发 + 摘要 + 替换) | compact.rs | ~150 行 |

---

## 7. 关键源码索引(速查)

| 主题 | 位置 |
|---|---|
| 会话文件命名/目录 | `rollout/src/recorder.rs:1549-1578`,`rollout/src/lib.rs:25-26` |
| 行格式 RolloutLine / RolloutItem | `protocol/src/protocol.rs:3379-3386` / `3184-3199` |
| SessionMeta / CompactedItem / TurnContextItem | `protocol.rs:3057-3114` / `3220-3236` / `3262-3306` |
| 持久化白名单 | `rollout/src/policy.rs:9-21, 39-59, 87-183` |
| 后台写入/延迟物化/失败重试 | `recorder.rs:892, 1603-1617, 1651-1676, 1774-1804` |
| 读回与容错 | `recorder.rs:982-1045` |
| resume 重建(逆序扫描+正序重放) | `core/src/session/rollout_reconstruction.rs:113-440` |
| resume 入口与 token 恢复 | `core/src/session/mod.rs:1286-1401, 1485-1490` |
| 内存历史 ContextManager | `core/src/context_manager/history.rs:40-60, 124-146, 297-315` |
| token 估算启发式 | `history.rs:481-506` |
| TokenUsage / TokenUsageInfo / BASELINE_TOKENS | `protocol.rs:2056-2137, 2213` |
| 90% 压缩阈值 | `protocol/src/openai_models.rs:459-470` |
| token_limit_reached 判定 | `core/src/session/context_window.rs:23-91` |
| 压缩触发点(采样后/前/模型切换) | `core/src/session/turn.rs:336-423, 845-875, 914-1008` |
| 压缩策略选择 | `turn.rs:1012-1089` |
| local 压缩全流程 | `core/src/compact.rs:240-393, 525-548, 611-685` |
| 压缩 prompt / 摘要前缀 | `prompts/templates/compact/prompt.md`, `prompts/templates/compact/summary_prefix.md` |
| 压缩检查点落盘 | `core/src/session/mod.rs:3188-3234` |
| 冷文件 zstd | `rollout/src/compression.rs:18, 29-31, 47-58` |
| SQLite 镜像定位 | `state/src/lib.rs:1-5`;兜底逻辑 `recorder.rs:437-702` |
| 全局消息历史 | `message-history/src/lib.rs:1-15, 52` |
