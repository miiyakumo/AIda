# CLI、Web 与 App 多端架构调研

> 调研日期：2026-07-30  
> Grok Build 本地快照：`500129c714ad1b10e6095481f4a8387a2ec52649`  
> Codex 本地快照：`61a44880a85d2fd0d8770908dea5733495e571c8`

## 1. 问题与裁决

本报告回答：当 Alda 音乐 Agent 同时提供 CLI、Web 和未来 App，且 Web/App 可能优先时，M0–M5 的 CLI 单进程架构应如何调整。

**架构裁决**：Web/App 优先是合理的产品方向；底层应从第一天就是宿主无关的 Agent Runtime，并通过统一的双向 Application Protocol 服务不同客户端。CLI 保留为调试、自动化、评测和高级用户入口，但不能继续作为领域逻辑、会话状态和审批流程的唯一宿主。

这不意味着首期复制 Codex 的完整 app-server。首期只实现支撑一个真实 Web 纵切片所需的最小协议、Session 单写者、结构化事件、断线恢复和 Artifact 读取；多租户、远程机器管理、完整订阅系统和协议代码生成按真实需求递增。

## 2. 证据边界

- **当前事实**：来自仓库内上述固定快照；没有在线核验其托管产品后端。
- **设计决策**：本文对 Alda Agent 的推荐，不代表代码已经实现。
- **待决策**：本地优先、云托管或混合部署尚未由产品需求确定；该选择会改变认证、文件访问、音频渲染和运维边界。

## 3. Grok Build 的多宿主设计

### 3.1 可移植 Agent 与多个宿主

Grok Build 根 README 将运行方式明确分为全屏 TUI、headless 和通过 Agent Client Protocol（ACP）嵌入编辑器。`xai-grok-agent` 又把工具、系统提示词、压缩策略和模型配置收敛为可由 shell、进程内宿主或 batch runner 消费的 `Agent` 对象。

可借鉴的不是其具体工具集合，而是“Agent 定义不属于某一个 UI”。Alda 版的 Brief、Constraint、Tool Registry、Provider 和创作循环同样不应依赖终端打印、Axum WebSocket 或前端组件。

### 3.2 ACP 是双向宿主协议

`xai-acp-lib` 用 gateway/channel 把 ACP 的 Agent Side 与 Client Side 解耦。协议不仅有 `prompt`、`cancel` 和 Session 通知，还允许 Agent 反向请求客户端执行：

- 请求权限；
- 读写文本文件；
- 创建、读取和终止终端；
- 发送 Session Notification。

这说明丰富客户端不是被动日志查看器。对音乐 Agent，同类反向交互包括请求审批、请求试听、询问用户、获取客户端播放能力以及采集确切 played range。

### 3.3 Session 与 Chat State 使用 Actor 单写者

`xai-chat-state` 把 conversation、sampling config、prompt index 和 token usage 放入专用 Tokio task；调用方持有可克隆 Handle，通过 command 和 oneshot query 访问状态。`SessionHandle` 同样是 `Clone + Send` 的 Session Actor 代理。

这个模型适合多客户端和异步工具：状态由单写者串行变更，不需要把全局 `Arc<Mutex<Session>>` 跨 `await` 传播。Alda 版应进一步把聊天状态与作品状态分开：Chat Actor 管上下文，Project Coordinator 管 Revision、Artifact、Audition 与领域事件。

### 3.4 WebSocket、持久 Agent 与 Leader

`agent/server.rs` 提供带认证的 WebSocket ACP Server。单个 `MvpAgent` 在持久线程中存活，连接断开后 Agent 和进行中的 Session 继续运行，新连接可重新接入。Leader 层还处理本机多个客户端、Session 恢复和进程升级。

Leader 协议的客户端能力值得直接吸收其设计意图：终端、文件读取、文件写入等能力按客户端声明，而不是假定所有 UI 都能访问运行时资源；代码注释明确举出 TUI 与 Web Client 的能力差异。

但不应照搬以下实现细节：

- 多处 unbounded channel 不适合作为我们的默认背压策略；
- `agent/server.rs` 的当前 relay 目标是可替换的单个连接，断线时部分出站消息会丢弃，不等于完整多订阅 Web 后端；
- Leader、远端 relay、Workspace Hub 和更新恢复解决的是成熟 Coding Agent 的部署问题，首期音乐 MVP 不需要全部引入；
- `xai-grok-shell` 聚合了大量职责，不应成为我们的大型“万能 runtime crate”。

## 4. Codex 的 app-server 设计

### 4.1 Core 与 Rich Client 之间有正式应用服务层

Codex 通过 `codex app-server` 支撑 VS Code 等丰富界面。对外协议使用 JSON-RPC 风格的双向消息，核心对象为：

```text
Thread → Turn → Item
```

客户端用 request 启动/恢复 Thread 和 Turn，服务端用 notification 流式发送 Item 与 Turn 状态；审批则是 Server → Client request，客户端必须回复决定。这比“REST 创建任务 + SSE 打字流”更贴合有审批、取消、工具调用和长任务的 Agent。

### 4.2 同一协议语义支持多种传输

app-server 支持 stdio、WebSocket、Unix Socket，并提供进程内运行路径。`codex-app-server-client` 让 TUI 和 exec 走类型化的进程内 channel，但响应仍保留 JSON-RPC envelope 语义，避免本地 CLI 和远端客户端形成两套业务契约。

这是本项目最值得借鉴的一点：CLI 可以为了低延迟使用进程内 typed transport，Web 使用 WebSocket，但两者必须看到同一组 Command、Event、Error 和审批状态。

### 4.3 多连接订阅、恢复与待决请求

app-server 以 `ConnectionId` 区分连接，并把 Thread 事件定向发送给订阅连接。Thread start/resume/fork 会建立订阅；断开连接不会把 Thread 与作品生命周期等同销毁。待决的 Server Request 也能在重新连接 Thread 时恢复投递，避免审批因 UI 断线永久悬挂。

这比把 WebSocket 本身当 Session 更可靠。浏览器标签页、移动端切后台和网络切换都是常态，作品 Session 必须独立于连接生命周期。

### 4.4 背压是协议行为，不只是 channel 选型

Codex 在传输入口、请求处理和出站写入之间使用 bounded queue；过载返回显式可重试错误。进程内客户端还区分必须交付与可丢弃事件：Turn 完成、权威 Item、正文 delta 等不能静默丢失，部分进度事件可降级并发出 `Lagged`。

音乐版也必须定义事件等级：

- **必须交付或可从权威投影恢复**：Revision/Artifact 创建、Constraint 结果、审批请求与决定、Turn 完成、Audition 开始/结束、ListeningFeedback、Accept/Publish；
- **可合并或丢弃**：token 级 UI 动画、播放光标、临时进度、重复状态摘要；
- **断线补偿**：客户端按 Session event sequence 或投影版本补读，不能假定重新连接后继续收到全部旧 delta。

### 4.5 不应直接照搬的部分

- Codex app-server 协议面已经非常大，包含 Coding Agent 的文件、终端、配置、环境与远程控制接口；
- 其 WebSocket 在当前 README 中仍标为 experimental/unsupported，并拒绝带 `Origin` 的请求，不能直接当浏览器生产方案；
- 完整的 Thread Store、daemon、远程机器更新与历史分页超出首期 Alda 项目规模；
- Thread/Turn/Item 不足以表达音乐作品真相，仍需 Project、Revision、Take、Artifact 和 Audition 领域模型。

## 5. 对比结论

| 维度 | Grok Build | Codex | Alda Agent 选择 |
|---|---|---|---|
| UI 解耦 | ACP + portable Agent | app-server + core | Runtime 不依赖任何 UI |
| 本地 CLI | TUI/headless/stdio | TUI/exec 走进程内 app-server | 进程内 typed client，共用协议语义 |
| Rich Client | ACP/WebSocket/Leader | JSON-RPC app-server | WebSocket 双向控制流 + HTTP Artifact |
| 状态并发 | Session/Chat Actor | Thread 管理器与订阅 | Chat Actor + Project Coordinator 单写者 |
| 客户端能力 | terminal/fs 等逐客户端声明 | initialize capabilities | 增加 playback/render/upload/device 能力 |
| 断线 | Agent 存活，load/replay | Thread 独立于连接、可重订阅 | Session 独立于连接，按序号补读 |
| 背压 | 部分路径仍是 unbounded | bounded + overload + 事件分级 | 全链 bounded，显式 Lag/Retry |
| 领域对象 | Session/Conversation | Thread/Turn/Item | Project/Revision/Audition + Thread/Turn/Item |

## 6. 推荐目标架构

```text
CLI ── InProcessTransport ─┐
                           │
Web/App ─ WebSocket ───────┼─ Application Protocol ─ App Service
Web/App ─ HTTP Artifact ───┘                         │
                                                    ├─ Session/Turn Runtime
                                                    ├─ Project Coordinator
                                                    ├─ Provider + Tool Runtime
                                                    ├─ Event/Projection Store
                                                    └─ Artifact Store
```

### 6.1 边界

1. **Domain**：Brief、Constraint、Revision、Take、Audition、Feedback；不依赖 Tokio、HTTP、CLI、Provider。
2. **Runtime**：Agent Loop、工具编排、取消、权限、资源锁；只发结构化事件，不打印终端。
3. **Application Service**：处理客户端命令、订阅、幂等、投影查询和 Server Request。
4. **Protocol**：版本化 Command/Event/Error/Capability；可生成 TypeScript 类型，但 Rust 领域实体不直接作为永久 wire schema。
5. **Transport**：进程内 bounded channel、WebSocket、测试 transport；HTTP 主要负责登录、快照查询、上传和 Artifact bytes。
6. **Clients**：CLI、Web、App 只持有显示状态和未提交草稿，不拥有作品权威状态。

### 6.2 最小协议对象

首个 Web 纵切片只需要：

- `initialize(client_info, capabilities, protocol_version)`；
- `project/create|open|snapshot`；
- `session/start|resume|subscribe`；
- `turn/start|steer|cancel`；
- `approval/respond`；
- `audition/start|progress|stop|feedback`；
- `artifact/get_manifest`，二进制内容走 HTTP；
- `event/resume(after_sequence)`。

每个会改变状态的客户端命令携带 `client_command_id`；作品提交另带 `base_revision_id`。服务端保证同一 command id 幂等，并以 CAS 拒绝过期基线。

### 6.3 客户端能力

不能只协商模型或工具能力，至少应预留：

- `score_preview`；
- `midi_playback` / `audio_playback`；
- `playback_progress_reporting`；
- `file_upload` / `local_file_access`；
- `microphone_input`；
- `external_midi_device`；
- `background_audio`；
- `interactive_approval`。

服务端根据能力选择动作。例如浏览器可播放 MIDI 时，服务端生成 `MidiRenderArtifact` 并请求客户端试听；无播放能力的 CLI 则可走受控本机 Alda Player。

### 6.4 Session、连接与作品不能混为一体

```text
Connection：短命，可断线、重连、多标签页
Session/Thread：对话和 Agent 工作上下文
Project：长期作品身份
Revision/Artifact：不可变作品事实
Audition：某客户端对某 Artifact 的确切试听
```

连接断开不取消 Turn，除非产品策略明确要求；取消由显式命令或无人订阅超时策略触发。客户端恢复时先读取权威投影，再从 sequence 补事件，而不是依赖内存 delta 重放。

## 7. 部署决策门

在实现真实 Web 登录和音频链前必须选择首发部署形态：

| 形态 | 优点 | 主要代价 |
|---|---|---|
| 本地服务 + 浏览器/PWA | Alda、文件和隐私留在本机 | 安装、端口、升级、浏览器本地认证 |
| 云托管 | 零安装、多端同步 | 多租户隔离、成本、素材外发、服务端音频后端 |
| 桌面壳 + 本地 Runtime | 本地能力和 Rich UI 最强 | 跨平台打包、移动端仍需另一方案 |
| 混合 | 可兼顾本地执行与云同步 | 协议、冲突、认证和运维最复杂 |

当前建议先用“本地 Runtime + Web/PWA”验证创作闭环，同时让协议不绑定本地路径；若产品确定云优先，再单独设计租户、对象存储、作业调度和数据驻留，不能用单用户 Actor 直接冒充云隔离。

## 8. 对现有路线的影响

M0–M5 仍应先建立单 Agent 基线，但“CLI composition root”需要改成“Application composition root，CLI 是第一个 client”。建议在 M1 前增加一个平台纵切片：

1. Runtime 用 fake Provider 产生结构化 Turn Event；
2. CLI 通过进程内 transport 消费同一协议；
3. Web 通过 WebSocket 启动/取消 Turn；
4. 断线后按 Session ID 与 sequence 恢复；
5. Artifact 通过 HTTP 下载；
6. 审批以 Server Request 往返，不阻塞终端 stdin。

验证该纵切片后再接真实 Provider 和 Alda，可以尽早发现协议、播放位置、断线和权限模型中的结构性错误，同时不需要先造完整 Web 产品。

## 9. 仍待回答的问题

1. 首发是本地 Web/PWA、云 Web，还是桌面壳？
2. Turn 在所有客户端断线后继续多久？
3. 一个 Project 是否允许多个用户同时写，还是首期只允许单写、多读？
4. 浏览器首期播放 MIDI、服务端离线音频，还是两者并存？
5. 哪些流事件必须逐条保存，哪些只保留最终投影？
6. 协议采用 JSON-RPC、普通 tagged message，还是兼容 ACP 子集？

