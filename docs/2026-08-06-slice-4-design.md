# 切片 4：项目、版本与交互式修改 — 设计文档

> 日期：2026-08-06
>
> 需求基线：`docs/requirements/product-requirements.md` §5–§9
>
> 状态：已实现并按进度审计修订

## 1. 目标

在切片 3 的一次性 `create` 命令之上，交付：

- **项目视图**：默认项目目录 + 显式路径，项目列表
- **持久化**：素材、音乐构想、线性版本、当前乐谱的本地文件存储
- **交互式 REPL**：自然语言对话 + 系统命令（play/stop/export/history/restore/quit）
- **修改闭环**：用户反馈 → Agent 修改 → 校验 → 新版本

## 2. 设计约束

- 保持单个 Rust crate，不拆分子 crate
- 不引入数据库、事件溯源、HTTP 服务
- 项目保留素材、要求和版本历史；新的修改请求从这些稳定事实重建干净上下文
- 只有当前自动修正或澄清往返保留短期消息上下文，完成后清空
- 实现显式 `/reload`；人工编辑只有重新读取并通过校验后才成为新版本

## 3. 架构新增

```
src/
  main.rs        ← 调度：list | repl（含 --project） | create | doctor | smoke
  repl.rs        ← 交互循环、命令解析、上下文组装、调用 Agent
  project.rs     ← 目录结构、Project 结构体、序列化、版本管理
  agent.rs       ← 扩增：新增 modify() 方法
  lib.rs         ← 扩增：更新 Command enum
  (其余不变)
```

### 3.1 模块边界

| 模块 | 职责 | 不做什么 |
|------|------|----------|
| `project.rs` | `Project` 结构体、`Versions` 管理、目录初始化、项目列表 | 不处理 Agent 逻辑、不访问网络 |
| `repl.rs` | 读行循环、命令分发、上下文组装、调用 `Agent::create`/`Agent::modify`、调用 `AldaRunner` | 不直接操作文件（委托给 `project.rs`） |
| `agent.rs` | 原 `create()` + 新增 `modify()`，共享内部消息构建和工具校验循环 | 不持有项目状态 |

## 4. 项目持久化 (`project.rs`)

### 4.1 项目定位

- **默认目录**：`~/.alda-agent/projects/<name>/`
- **显式路径**：`alda-agent --project /path/to/dir`
- **列表**：`alda-agent list` — 枚举 `~/.alda-agent/projects/` 下的子目录名

### 4.2 目录布局

```
project-dir/
├── project.json    ← 元数据（素材、解读、当前版本号、配器、版本摘要表）
├── current.alda     ← 当前工作乐谱（覆盖式更新，创建修改时的中间态）
├── versions/
│   ├── 0001.alda
│   └── 0002.alda
└── exports/
```

### 4.3 `project.json` 格式

```json
{
  "project_name": "机械硬盘之诗",
  "source_material": "飞旋，飞旋！\n在区块间架起长桥。...",
  "interpretation": "作品以机械运动为核心意象...",
  "current_version": 2,
  "mode": "full",
  "versions": {
    "1": {
      "created_at": "2026-08-06T15:30:00Z",
      "summary": "首次创作",
      "checks_passed": true
    },
    "2": {
      "created_at": "2026-08-06T16:00:00Z",
      "summary": "中段更冰冷，结尾加速明亮",
      "checks_passed": true
    }
  }
}
```

### 4.4 版本管理规则

1. **首次通过检查** → `save_version(alda_code, summary)`：写入 `versions/0001.alda`，更新 `project.json`
2. **成功修改** → 同上，版本号取历史最大值加一
3. **失败修改** → 不创建版本，不修改 `project.json`，不覆盖 `current.alda`
4. **恢复** → `restore_version(n)`：把 `versions/{n:04}.alda` 复制到 `current.alda`，更新 `current_version`
5. 版本号从 1 开始单调递增

## 5. REPL (`repl.rs`)

### 5.1 CLI 入口

```
alda-agent list                         → 列出默认目录下已有项目
alda-agent --project /path/to/project   → 进入指定项目 REPL
alda-agent create [opts]                → 一次性快速创作（保留）
alda-agent doctor                       → 保留
alda-agent smoke                        → 保留
```

不带 `--project` 的裸 `alda-agent` 进入交互模式前先列出可用项目并提示选择或新建。

### 5.2 REPL 命令

| 命令 | 行为 |
|------|------|
| `/play` | 播放 `current.alda` |
| `/stop` | 停止播放 |
| `/export` | 导出 `.alda` + MIDI 到 `exports/` |
| `/history` | 列出所有版本号、创建时间、修改摘要 |
| `/restore N` | 恢复版本 N 为当前工作基线 |
| `/quit` | 退出 |

### 5.3 对话模型

- 输入以 `/` 开头 → 命令分发
- 输入不以 `/` 开头 → 自然语言：
  - 尚无当前乐谱 → 视为创作请求（调用 `Agent::create`）
  - 已有当前乐谱 → 视为修改/反馈（调用 `Agent::modify`）
- 新的修改请求只携带原始素材、当前乐谱、最新反馈和仍有效的显式约束，不回灌旧生成过程、
  失败草稿或工具回执
- 模型在干净请求内区分明确局部修改与整体审美反馈：前者尽量保持其余内容，后者允许重构；
  范围确实不清且会显著改变作品时只提出一个澄清问题
- 自动修正和澄清回答沿用当前短期消息上下文；生成成功后清空
- 每轮 Agent 返回后，成功则创建新版本，失败则保留现状

### 5.4 状态机

```
          启动
           │
    ┌──────┴──────┐
    │ 无 project  │ → 提示选/建项目（list | new）
    └──────┬──────┘
           │
    ┌──────┴──────┐
    │ 已有 project│
    └──────┬──────┘
           │
    ┌──────┴──────────┐
    │ 无 current.alda │ → 等待创作输入
    └──────┬──────────┘
           │
    ┌──────┴──────────┐
    │ 有 current.alda │ → 创作/修改 循环
    └─────────────────┘
```

## 6. Agent 扩增 (`agent.rs`)

### 6.1 新增 `ModifyRequest` 和 `modify()` 方法

```rust
pub struct ModifyRequest {
    pub source_material: String,   // 原始素材
    pub current_alda: String,      // 当前有效版本的内容
    pub feedback: String,          // 最新自然语言反馈
    pub mode: CreationMode,
    pub target_duration_secs: Option<f64>,
    pub included_instruments: Vec<String>,
    pub excluded_instruments: Vec<String>,
    pub max_rounds: usize,
}
```

`modify()` 方法内部流程与 `create()` 类似：

1. 组装干净消息：`system` → `user`（原始素材 + 当前乐谱 + 最新反馈 + 有效约束）
2. 调用 `chat_stream`，收集文本和工具调用
3. 校验、反馈、最多 3 轮自动修正
4. 返回 `CreationResult`（复用，含 alda_code + checks + interpretation）

### 6.2 与 `create()` 的区别

| | `create` | `modify` |
|---|---|---|
| 用户消息格式 | 素材 + 创作模式 | 当前乐谱 + 修改指令 + 历史摘要 |
| 系统提示 | 同一份 | 同一份，可考虑注入"你正在修改已有作品" |
| 校验流程 | 相同 | 相同 |
| 返回类型 | `CreationResult` | 复用 `CreationResult` |

## 7. 文件变更清单

| 文件 | 变更类型 |
|------|----------|
| `src/project.rs` | **新增** — 项目结构体、序列化、版本管理、列表 |
| `src/repl.rs` | **新增** — 交互循环、命令分发、上下文组装 |
| `src/agent.rs` | **修改** — 新增 `ModifyRequest`、`modify()` 方法 |
| `src/lib.rs` | **修改** — 更新 `Command` enum（`List`、`Repl`） |
| `src/main.rs` | **修改** — 新增 `list` 和交互式 REPL 路径 |

## 8. 非目标

- 不实现分支、Take、合并
- 不实现对话摘要/压缩
- 不实现多项目同时打开
- 不引入配置文件（项目名从目录名推导）
- 列表命令只支持默认目录，不支持跨目录搜索

## 9. 测试策略

| 测试内容 | 类型 | 说明 |
|----------|------|------|
| `Project` 序列化/反序列化 | 单元 | JSON 读写, 版本元数据 |
| `save_version` / `restore_version` | 单元 + 集成 | 文件系统操作, 版本号递增 |
| 恢复后 `current_version` 正确更新 | 单元 | |
| 失败修改不创建版本 | 单元 | |
| REPL 命令解析 | 单元 | `/` 前缀 vs 自然语言 |
| 重启恢复 | 集成 | 写项目 → 重启 → 读取一致 |
| 路径逃逸、损坏 JSON 负例 | 集成 | |

真实 DeepSeek 和播放的交互行为留到切片 5 整体验收。
