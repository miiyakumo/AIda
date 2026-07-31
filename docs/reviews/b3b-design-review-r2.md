---
verdict: approved
scope: design
artifact: /home/mii/code/draft/docs/plans/mvp-deliberative-execution.md
reviewer: B3b fresh-context independent design reviewer R2
date: 2026-07-31
---

# B3b 独立设计审查 R2

## 结论

B3b 修订设计可批准实施，verdict 为 `approved`。

R1 的 M1–M5 均已闭合：Session rollout明确冻结
`stream_id = canonical SessionId`；canonical prompt进入authoritative
`TurnStartedV1`事实；startup reconciliation区分runtime work、pending input与cancel
路径并用单batch提交；command-only batch采用唯一空区间且batch/event head分离；生产ID
改为带类型前缀的128-bit CSPRNG，并以descriptor-relative全量Session枚举重建跨Session
owner index。

本轮未发现新的重大矛盾。B3b仍不接生产App Service，Session create的control catalog
原子编排明确留给B4；这与当前slice边界一致，而不是遗漏。实现验收必须保留§16列出的
固定向量、重启矩阵、目录异常与failpoint门禁。

## R1 M1–M5闭合核验

### M1 — 已闭合：stream identity与冻结wire一致

- §16明确规定Session rollout的`stream_id`就是canonical `SessionId`，epoch固定1，
  不再生成snapshot无法表达的随机stream ID。
- 该规则与现有`EventResume`把cursor stream ID作为Session ID定位的A4语义一致，不需
  扩展`SessionSnapshot` wire schema。
- 目录hash、stored Session ID和writer lease key必须三方一致，并要求
  start→snapshot→resume→restart固定向量证明identity不变。

### M2 — 已闭合：prompt成为可replay的authoritative context

- stored `TurnStartedV1`必须保存canonical prompt，并冻结UTF-8及1..=8000 bytes边界。
- projection保留prompt但映射wire event时不泄漏；checkpoint只能缓存log已有事实，不能
  补造context。
- 验收明确要求Question Pending重启后回答所生成approval digest与不中断路径逐字节相同，
  直接覆盖R1指出的权限绑定风险。

### M3 — 已闭合：restart reconciliation有完整收敛规则

- `Running`且无pending runtime object转为`AbortedByRestart`；`CancelRequested`按
  created sequence追加owner-abort后转`Cancelled`；合法`WaitingForInput`保留并重投，
  无或非唯一pending则判projection corrupt。
- `BudgetExceeded`和`AbortedByRestart`被纳入正式terminal集合；terminal与pending/
  execution fact的reducer约束保持fail closed。
- reconciliation使用
  `restart-v1:<state-instance-id>:<session-id>:<pre-head>`稳定transaction ID，并以单一
  batch提交，因此中途崩溃只有全有或全无；重复startup由新projection状态或同transaction
  去重。
- 验收覆盖Running、CancelRequested、合法/非法WaitingForInput、BudgetExceeded及
  reconciliation中途崩溃，不再只覆盖三个happy restart切点。

### M4 — 已闭合：空批数学、anchor和无stream路由边界均已冻结

- command-only batch定义
  `event_count=0, first_sequence=current_head+1, last_sequence=current_head`，这是唯一合法
  空区间；它不推进event head，但推进batch checksum、offset和transaction chain。
- checkpoint以covered offset/sequence/checksum三者锚定，连续空批及空批夹事件按batch
  checksum验证，避免多个batch共享event head时产生歧义。
- 已定位可信Session的无事实稳定回复写入该Session；ownership mismatch写入请求声明且
  已验证存在的Session stream，不污染实际owner stream。
- SessionStart、解析失败、not-found及无法定位可信Session的ownership错误明确不在B3b
  per-Session durable承诺内。Session create command→Session分配属于B4 control
  catalog；B3b在B4完成前不接生产create，范围清晰。
- 验收加入连续空批、checkpoint跨同head多batch、事件/空批混排及无可信Session不伪称
  durable的向量。

### M5 — 已闭合：随机ID、fd枚举和全局owner来源明确

- 生产Session/Turn/Question/Approval ID改为带类型前缀的128-bit CSPRNG小写hex；内存
  fixture可继续使用可预测ID，不改变wire类型。
- 新ID必须先对durable catalog/Session projection及全局owner index做碰撞检查，有界
  重试耗尽返回typed internal error。
- `list_sessions()`从sessions fd枚举64字符小写hex目录，以
  `O_DIRECTORY|NOFOLLOW|NONBLOCK`逐项打开，在同一handle验证owner/private，并从trusted
  replay取得canonical Session ID后重算目录hash。
- B4必须在接受命令前完成全量catalog/owner index重建；重复Session ID、跨Session重复
  Turn/Question/Approval ID、hash错配或异常目录均fail closed，owner不再来自内存counter
  或文件名猜测。
- 验收覆盖多Session重启后继续分配、随机碰撞上限、重复owner、hash错配和weak/special
  目录。

## 新矛盾检查

未发现阻断实施的新问题：

- Project与Session具有独立registry、文件、event enum、epoch和sequence，不存在万能流
  拼接。
- primitive-only stored codec、exact reply、poisoned writer、repair及checkpoint规则与
  B3a接口边界相容。
- cursor只读取authoritative event head，不把command-only batch误报成新wire event。
- restart planner是纯计划器，B3b不越界执行Provider/Alda或直接接生产Coordinator。
- retention/compaction明确排除，epoch固定1，与A4当前无retention gap语义一致。

## 非阻断实施注意

### I1 — directory enumeration必须把“只枚举”实现为异常entry fail closed

§16同时要求只接受64字符小写hex目录和异常目录不静默跳过。实现时不能用filter直接忽略
sessions目录中的非canonical entry；除协议明确拥有并可验证清理的staging entry外，未知
entry、symlink或特殊文件都应产生startup错误。验收中的weak/special目录应扩展一个
non-hex unknown entry fixture。

### I2 — B4 create catalog的跨aggregate恢复仍是后续门控

B3b正确地不承诺SessionStart的per-Session归档。B4实施时仍需证明control catalog已提交、
首个Session batch尚未提交这一崩溃切点可重入，并最终返回同一Session ID与exact reply。
这不阻断B3b的独立rollout codec/replay/writer实施，但在B4 RELEASE前不能省略。

## 实施门控

- 固定A2 happy/deny/question-cancel/approval-cancel event vectors与在线projection逐字段
  对比。
- 执行§16全部restart reconciliation、command-only/checkpoint和cursor truth-table
  矩阵。
- 对目录枚举、全局重复owner、random collision、stored prompt/choice/digest篡改及完整
  crash/failpoint矩阵做fail-closed验证。
- B3b实现不得接生产App Service或提前实现B4 control catalog。
- L3必须包含B3a/B2/B1/Slice A及全仓Rust、Node、JS syntax、diff门禁。

## 审查判定

- R1 M1：PASS。
- R1 M2：PASS。
- R1 M3：PASS。
- R1 M4：PASS。
- R1 M5：PASS。
- 新重大问题：无。
- B3b设计：**APPROVED FOR IMPLEMENTATION**。
