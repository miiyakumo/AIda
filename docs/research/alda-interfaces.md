# Alda 可编程接口全盘点

> 基于仓库内 `ref/alda/` 源码查证, 所有引用采用 `文件:行号` 格式。校验基准: Alda 2.4.3, commit `33e17e5674fd98da89462f21b2e0e5f2d9f16944`。

---

## 一、接口总览

Alda 通过 **三层协议** 暴露接口:

| 层级 | 协议 | 编程入口 | 用途 |
|------|------|----------|------|
| CLI | 子命令 + 参数 | `alda <subcommand>` | 一次性调用 |
| nREPL | bencode over TCP | `alda repl --client --message '{"op": "..."}'` | 有状态交互 |
| OSC | OSC over TCP | `alda-player` 内部 | 客户端/REPL 服务器驱动播放器 |

LLM 可编程驱动的主要路径是 **CLI 子命令** 和 **nREPL ops**。OSC 层由 Alda 内部封装, 不直接暴露给外部。

---

## 二、CLI 子命令一览

### 2.1 输入方式

绝大部分 CLI 子命令支持三种输入方式, 优先级为 `--file` > `--code` > `stdin`:

| 参数 | 说明 |
|------|------|
| `-f`, `--file <path>` | 从文件读取 Alda 代码 `cmd/play.go:28-29` |
| `-c`, `--code <string>` | 直接提供代码字符串 `cmd/play.go:40-41` |
| stdin 管道 | 由 `system.ReadStdin()` 检测 `cmd/play.go:71-73`; `system/stdin.go:27-35` |

全局参数: `-v 0-3` 控制 verbosity (`cmd/root.go:236-238`).

### 2.2 play -- 解析并播放

**文件**: `cmd/play.go`

```bash
alda play -f score.alda
alda play -c "piano: c d e"
echo "piano: c d e" | alda play
alda play                    # 无参数: 相当于 unpause, 继续暂停的播放
```

| 参数 | 类型 | 说明 | 文件:行号 |
|------|------|------|-----------|
| `-f`, `--file` | string | Alda 源文件 | `cmd/play.go:36-37` |
| `-c`, `--code` | string | Alda 代码字符串 | `cmd/play.go:40-41` |
| `-i`, `--player-id` | string | 指定播放器进程 ID | `cmd/play.go:28-29` |
| `-p`, `--port` | int | 指定播放器端口 | `cmd/play.go:32-33` |
| `-F`, `--from` | string | 起始时间/标记 (e.g. `0:30`, `verse`) | `cmd/play.go:44-49` |
| `-T`, `--to` | string | 终止时间/标记 (e.g. `1:00`, `chorus`) | `cmd/play.go:52-57` |
| `-w`, `--wait` | bool | 阻塞等待播放完成 | `cmd/play.go:60-61` |

**输出**: stderr 打印 `"Playing..."` (`cmd/play.go:319`), stdout 无内容。退出的 exit code 指示成功(0)或失败(非0)。

**人耳专用** -- LLM 只能得到 success/failure 元信息, 无法感知听感。

**unpause 模式**: 当不提供任何代码输入时, 行为变为向所有 active 状态的 player 发送 `/system/play` OSC 消息 (`cmd/play.go:294-297`), 即从暂停位置继续播放。

### 2.3 parse -- 解析并输出结构化数据

**文件**: `cmd/parse.go`

```bash
alda parse -c "piano: c d e" -o data     # 默认: score JSON
alda parse -c "piano: c d e" -o events   # 事件数组 JSON
alda parse -c "piano: c d e" -o ast      # 语法树 JSON
alda parse -c "piano: c d e" -o ast-human # 人类可读 AST
```

| 参数 | 类型 | 说明 | 文件:行号 |
|------|------|------|-----------|
| `-f`, `--file` | string | Alda 源文件 | `cmd/parse.go:20-21` |
| `-c`, `--code` | string | Alda 代码字符串 | `cmd/parse.go:24-25` |
| `-o`, `--output` | string | 输出类型: `data`(默认)/`events`/`ast`/`ast-human` | `cmd/parse.go:28-29` |

**四种输出类型** (定义于 `cmd/parse.go:47-67`):

| 输出类型 | 内容 | 格式 | LLM 可用性 |
|----------|------|------|-----------|
| `data` | score 模型 JSON (parts, events, markers, current-parts 等) | 通过 `score.JSON().String()` 输出到 stdout | 可行 -- 结构化 JSON |
| `events` | 解析后的 scoreUpdate 事件列表 JSON 数组 | 通过 `json.Array()` 输出到 stdout | 可行 -- 结构化 JSON |
| `ast` | 语法树 JSON 对象 | 通过 `ast.JSON().String()` 输出到 stdout | 可行 -- 结构化 JSON |
| `ast-human` | 人类可读的语法树文本 | `parser.HumanReadableAST()` | 可读但解析较困难 |

**data 输出格式** (基于 `repl/client.go:896-944` 中 `printScoreInfo` 的字段访问):
- `parts`: 各部分及其乐器信息 (`"parts"` -> `"stock-instrument"`)
- `current-parts`: 当前活跃的声部列表
- `events`: 事件数量
- `markers`: 标记名称与偏移量 (毫秒)

**错误输出**: `model.AldaSourceError` 类型错误经由 `help.UserFacingError` 包装输出到 stderr, 包含源码行列信息 (`cmd/parse.go:135-138`).

### 2.4 export -- 导出 MIDI

**文件**: `cmd/export.go`

```bash
alda export -c "piano: c d e" -o three-notes.mid        # 写入文件
alda export -c "piano: c d e" > three-notes.mid          # stdout 重定向
alda export -c "piano: c d e" | some-process              # 管道给其他程序
```

| 参数 | 类型 | 说明 | 文件:行号 |
|------|------|------|-----------|
| `-f`, `--file` | string | Alda 源文件 | `cmd/export.go:28-29` |
| `-c`, `--code` | string | Alda 代码字符串 | `cmd/export.go:32-33` |
| `-F`, `--from` | string | 起始时间/标记 | `cmd/export.go:36-41` |
| `-T`, `--to` | string | 终止时间/标记 | `cmd/export.go:44-49` |
| `-o`, `--output` | string | 输出文件名 (缺失则 stdout) | `cmd/export.go:52-53` |
| `-O`, `--output-format` | string | 输出格式, 目前仅支持 `midi` | `cmd/export.go:56-57` |

**实现流程** (`cmd/export.go:90-259`):
1. 解析 Alda 代码 -> AST -> ScoreUpdates -> Score
2. 找到可用的 player 进程
3. 以 `LoadOnly()` 模式传输 score 到 player
4. 向 player 发送 `/system/midi/export` OSC 消息
5. 等待 MIDI 文件写入完成 (最多 60 秒, `cmd/export.go:22`)
6. 写入指定文件或 stdout
7. 向 player 发送 shutdown 消息

**输出**:
- stderr: `"Exporting..."` 进度提示 (`cmd/export.go:232`), 完成后 `"Exported score to <filename>"` (`cmd/export.go:252`)
- stdout (当 `-o` 未指定): 原始 MIDI 二进制数据 (`cmd/export.go:254`)

**LLM 可用**: MIDI 二进制数据可被程序解析 (音符、时长、速度、乐器等), 作为间接反馈信号。

### 2.5 stop -- 停止播放

**文件**: `cmd/stop.go`

```bash
alda stop                  # 停止所有已知 player
alda stop -p 27278         # 停止指定端口
alda stop -i <player-id>   # 停止指定 ID
```

| 参数 | 类型 | 说明 | 文件:行号 |
|------|------|------|-----------|
| `-i`, `--player-id` | string | 指定 player ID | `cmd/stop.go:14-18` |
| `-p`, `--port` | int | 指定 player 端口 | `cmd/stop.go:22-27` |

**输出**: stderr 打印 `"Stopping playback."` (`cmd/stop.go:38`)。向 player 发送 `/system/stop` OSC 消息 (`cmd/stop.go:68`)。

### 2.6 instruments -- 列出可用乐器

**文件**: `cmd/instruments.go`

```bash
alda instruments
```

**输出**: stdout, 每行一个乐器名称, 调用 `model.InstrumentsList()` 获取 (`cmd/instruments.go:15`)。

无任何参数。

### 2.7 ps -- 列出后台进程

**文件**: `cmd/ps.go`

```bash
alda ps
```

**输出**: stdout 输出 TSV 表格 (`cmd/ps.go:26-43`):
```
id    port    state    expiry    type    pid
<id>  <port>  <state>  <time>    player  <pid>
<id>  <port>  -        -         repl-server  <pid>
```

无任何参数。数据来源于:
- `system.ReadPlayerStates()` -- 读取 `CachePath("state", "players", <version>)` 下的 JSON 状态文件
- `system.ReadREPLServerStates()` -- 读取 `CachePath("state", "repl-servers")` 下的 JSON 状态文件

### 2.8 doctor -- 健康检查

**文件**: `cmd/doctor.go`

```bash
alda doctor                    # 完整检查 (含音频)
alda doctor --no-audio         # 跳过音频相关检查
```

| 参数 | 类型 | 说明 | 文件:行号 |
|------|------|------|-----------|
| `--no-audio` | bool | 禁用需要音频设备的检查 | `cmd/doctor.go:64-70` |

**执行的检查步骤** (`cmd/doctor.go:76-762`):

| # | 步骤 | 说明 | 文件:行号 |
|---|------|------|-----------|
| 1 | Parse source code | 解析测试输入 | `cmd/doctor.go:82-100` |
| 2 | Generate score model | 构建 score 对象 | `cmd/doctor.go:104-113` |
| 3 | Clean stale players | 清理已失效的 player 状态文件 | `cmd/doctor.go:117-149` |
| 4 | Find an open port | 找到一个可用端口 | `cmd/doctor.go:153-168` |
| 5 | Send and receive OSC | 测试 OSC 收发 | `cmd/doctor.go:177-219` |
| 6 | Locate alda-player | 在 PATH 上找到 `alda-player` | `cmd/doctor.go:224-263` |
| 7 | Check alda-player version | 版本一致性检查 | `cmd/doctor.go:266-301` |
| 8 | Spawn a player process | 启动 player (可能带 `--lazy-audio`) | `cmd/doctor.go:305-332` |
| 9 | Ping player process | ping player 确认可用 | `cmd/doctor.go:338-354` |
| 10 | Play score | 播放测试乐谱 (audio 时) | `cmd/doctor.go:358-368` |
| 11 | Export score as MIDI | 导出并验证 MIDI 内容 (audio 时) | `cmd/doctor.go:372-459` |
| 12 | Locate player logs | 找到 alda-player.log | `cmd/doctor.go:463-490` |
| 13 | Player logs show ping | 验证 ping 记录在日志中 | `cmd/doctor.go:494-518` |
| 14 | Shut down player | 关闭 player | `cmd/doctor.go:520-528` |
| 15 | Spawn player on unknown port | 启动第二个 player (不带端口) | `cmd/doctor.go:531-543` |
| 16 | Discover the player | 通过状态文件自动发现 | `cmd/doctor.go:549-598` |
| 17 | Ping the player | 再次 ping | `cmd/doctor.go:602-616` |
| 18 | Shut the player down | 关闭第二个 player | `cmd/doctor.go:620-648` |
| 19 | Start a REPL server | 启动 nREPL 服务器 | `cmd/doctor.go:653-670` |
| 20 | Find the REPL server | 通过状态文件发现 | `cmd/doctor.go:687-709` |
| 21 | Interact with REPL server | clone + describe 测试 | `cmd/doctor.go:714-731` |
| 22 | Shut down REPL server | 关闭并确认清理 | `cmd/doctor.go:735-760` |

**输出格式**: 每步以 `OK` (绿色) 或 `ERR` (红色) 开头 (`cmd/doctor.go:33-40`)。

### 2.9 repl -- REPL 客户端/服务器

**文件**: `cmd/repl.go`

```bash
# 交互式 REPL (server + client 一起启动)
alda repl
alda repl --client --server

# 仅启动服务器 (无交互)
alda repl --server --port 12345

# 连接已有服务器 (交互式)
alda repl --client --port 12345

# 发送一次性 JSON 消息 (非交互, 程序化调用)
alda repl --port 12345 --message '{"op": "eval-and-play", "code": "piano: c d e"}'
```

| 参数 | 类型 | 说明 | 文件:行号 |
|------|------|------|-----------|
| `-H`, `--host` | string | REPL 服务器主机名 (默认 `127.0.0.1`) | `cmd/repl.go:24-25` |
| `-p`, `--port` | int | REPL 服务器端口 | `cmd/repl.go:28-29` |
| `-c`, `--client` | bool | 启动 REPL 客户端 | `cmd/repl.go:32-33` |
| `-s`, `--server` | bool | 启动 REPL 服务器 | `cmd/repl.go:36-37` |
| `-m`, `--message` | string | 发送 JSON nREPL 消息 (程序化模式) | `cmd/repl.go:40-46` |

**程序化消息模式** (`cmd/repl.go:144-149`): 当指定 `--message` 时, 不会进入交互模式, 而是:
1. 解析 JSON 为 `map[string]interface{}`
2. 调用 `repl.SendMessage(host, port, msg)` (`cmd/repl.go:82`)
3. 输出 JSON 响应到 stdout (`cmd/repl.go:87`)
4. 检查响应中是否有错误, 有则以非零 exit code 退出 (`cmd/repl.go:89-96`)

`repl.SendMessage` 内部流程 (`repl/client.go:1160-1175`):
1. 建立 TCP 连接到 REPL 服务器
2. 发送 `clone` 请求获取 session ID
3. 发送 `describe` 请求验证服务器
4. 发送实际请求 (抑制错误打印)
5. 返回响应 map

### 2.10 shutdown -- 关闭后台进程

**文件**: `cmd/shutdown.go`

```bash
alda shutdown                  # 关闭所有已知 player
alda shutdown -p 27278         # 关闭指定端口
alda shutdown -i <player-id>   # 关闭指定 ID
```

**输出**: stderr 打印 `"Shutting down player processes."` (`cmd/shutdown.go:30`)。向 player 发送 `/system/shutdown` OSC 消息 (`cmd/shutdown.go:60`)。

### 2.11 version -- 版本信息

**文件**: `cmd/version.go`

```bash
alda version
```

**输出**: stdout: `alda X.X.X` (`cmd/version.go:14`)。版本号来自 `generated.ClientVersion`。

### 2.12 import -- 从 MusicXML 导入

**文件**: `cmd/import.go`

```bash
alda import -i musicxml -f score.musicxml -o score.alda
alda import -i musicxml -c "<musicxml..." > score.alda
```

| 参数 | 类型 | 说明 | 文件:行号 |
|------|------|------|-----------|
| `-f`, `--file` | string | 输入文件路径 | `cmd/import.go:19-21` |
| `-c`, `--code` | string | MusicXML 字符串 | `cmd/import.go:23-25` |
| `-o`, `--output` | string | 输出 .alda 文件名 | `cmd/import.go:27-28` |
| `-i`, `--import-format` | string | 输入格式, 目前仅 `musicxml` | `cmd/import.go:31-32` |

**实现流程**: MusicXML -> `importer.ImportMusicXML()` -> ScoreUpdates -> `parser.GenerateASTFromScoreUpdates()` -> `FormatASTToCode()` -> 输出 Alda 代码。

### 2.13 format -- 格式化 Alda 代码 (实验性)

**文件**: `cmd/format.go`

```bash
alda format -f score.alda
alda format -f score.alda -o        # 原地覆盖
alda format -f score.alda -w 120 -i "    "
```

目前标记为 Hidden, 且会丢弃注释 (`cmd/format.go:39-40`, `cmd/format.go:64-66`)。

### 2.14 telemetry / update

**文件**: `cmd/telemetry.go`, `cmd/update.go`

不涉及音乐功能, 略。

---

## 三、nREPL 接口 (bencode over TCP)

### 3.1 协议说明

**文档**: `doc/alda-repl-server-api.adoc:1-5`

- 传输: TCP, 协议为 [nREPL](https://nrepl.org), 编码为 [bencode](https://en.wikipedia.org/wiki/Bencode)
- 也可以绕过 bencode 直接通过 `alda repl --client --message` 发送 JSON (`doc/alda-repl-server-api.adoc:8-22`)

**请求格式**: `{"op": "<操作名>", "session": "<session-id>", "id": "<message-id>", ...}`

**响应格式**: `{"id": "<message-id>", "session": "<session-id>", "status": ["done"], ...}` 或 `{"id": "...", "status": ["done", "error"], "problems": ["..."], ...}`

### 3.2 全部 nREPL Ops

定义于 `repl/server.go:360-530`, ops 路由表。

#### clone

```
请求: {"op": "clone"}
响应: {"status": ["done"], "new-session": "<uuid>"}
```
`repl/server.go:364-368` -- nREPL 协议要求, 生成新 session ID。Alda 的 session 仅对 server 端状态有意义。

#### describe

```
请求: {"op": "describe"}
响应: {"status": ["done"], "versions": {"alda": {"version-string": "X.X.X"}}, "ops": {...}}
```
`repl/server.go:371-373` -- 返回服务器版本信息和可用操作列表。`describeResponse` 定义于 `repl/server.go:339-345`, ops 列表定义在 `repl/server.go:347-358`。

#### eval (占位)

```
请求: {"op": "eval"}
响应: {"status": ["done"], "value": "¯\\_(ツ)_/¯"}
```
`repl/server.go:378-380` -- nREPL 协议适配, 无实际功能。

#### eval-and-play

```
请求: {"op": "eval-and-play", "code": "<Alda code string>"}
响应: {"status": ["done"]} 或 {"status": ["done", "error"], "problems": ["<error message>"]}
```
**必填参数**: `code` (string) -- 需要 `requestFieldSpec{name: "code", valueType: typeString, required: true}` (`repl/server.go:384-386`)

`repl/server.go:382-400` -- 最核心的操作。等价于 REPL 交互中每输入一行。流程 (`repl/server.go:616-636`):
1. 服务端调用 `updateScoreWithInput(code)` 解析并追加代码
2. 依次执行: `parser.ParseString` -> `ast.Updates()` -> `score.Update()`
3. 将输入追加到 `server.input`
4. 通过 OSC 发送新事件到 player 播放

**人耳专用**: LLM 调用后只能从响应的 `status` 和 `problems` 判断成败, 不能评估听感。

#### export

```
请求: {"op": "export"}
响应: {"status": ["done"], "binary-data": "<MIDI 二进制数据的字符串形式>"}
```
`repl/server.go:402-410` -- 导出当前 score 为 MIDI 数据。实现 (`repl/server.go:718-771`):
1. 调用 `server.reload()` 重新加载 score (包含速度消息)
2. 创建临时文件
3. 通过 OSC 发送 `/system/midi/export` 到 player
4. 等待 MIDI 文件写入 (最多 20 秒, `repl/server.go:29`)
5. 读取文件并返回字节数据

**⚠注意**: binary-data 以 Go string 类型返回 (`repl/client.go:103-114`), 不是 byte array, 实际使用时需要 `[]byte(res["binary-data"].(string))` 转换。

#### instruments

```
请求: {"op": "instruments"}
响应: {"status": ["done"], "instruments": ["piano", "violin", ...]}
```
`repl/server.go:412-416` -- 调用 `model.InstrumentsList()` 返回可用乐器列表。

#### load

```
请求: {"op": "load", "code": "<Alda code string>"}
响应: {"status": ["done"]} 或 {"status": ["done", "error"], "problems": ["..."]}
```
**必填参数**: `code` (string) (`repl/server.go:420-422`)

`repl/server.go:418-436` -- 解析输入为新 score 并加载到 REPL 服务器。会先 `resetState()` 再 `updateScoreWithInput()`, 并以 `LoadOnly()` 模式传输 (不触发播放, `repl/server.go:650`)。

#### new-score

```
请求: {"op": "new-score"}
响应: {"status": ["done"]} 或 {"status": ["done", "error"], "problems": ["..."]}
```
`repl/server.go:438-445` -- 调用 `server.resetState()` 重置服务器状态, 包括关闭当前 player 并创建空白 score (`repl/server.go:108-119`)。

#### replay

```
请求: {"op": "replay", "from": "0:05", "to": "verse"}  // from/to 可选
响应: {"status": ["done"]} 或 {"status": ["done", "error"], "problems": ["..."]}
```
**可选参数** (`doc/alda-repl-server-api.adoc:127-131`):
- `from` -- 起始时间 (mm:ss) 或标记名
- `to` -- 终止时间 (mm:ss) 或标记名

`repl/server.go:447-472` -- 从头播放当前 score 的选定片段。实现 (`repl/server.go:683-711`): resetState + evalAndPlay, 时间范围内的事件会被重新调度。与 incremental eval-and-play 不同, replay 包含完整的速度消息 (因为 `syncOffset == 0`)。

**人耳专用** -- 与 eval-and-play 相同。

#### score-data

```
请求: {"op": "score-data"}
响应: {"status": ["done"], "data": "<JSON 字符串>"}
```
`repl/server.go:474-478` -- 返回 `server.score.JSON().String()`, 即当前 score 的完整数据表示。

客户端解析: `json.ParseJSON([]byte(res["data"].(string)))` (`repl/client.go:816-833`)。

**LLM 可用**: 结构化 JSON, 包含 `parts`(乐器/声部), `events`(事件数), `markers`(标记偏移), `current-parts`(当前活跃声部) 等字段 (从 `repl/client.go:896-944` 可见)。

#### score-events

```
请求: {"op": "score-events"}
响应: {"status": ["done"], "events": "<JSON 字符串>"} 或 {"status": ["done", "error"], "problems": ["..."]}
```
`repl/server.go:480-499` -- 重新解析 `server.input` 并返回 event 格式的 JSON 数组。等价于 `alda parse -o events`。

客户端解析: `json.ParseJSON([]byte(res["events"].(string)))` (`repl/client.go:835-852`)。

**LLM 可用**: 每个 event 是结构化对象, 可精确分析音符、乐器、时长等。

#### score-ast

```
请求: {"op": "score-ast"}
响应: {"status": ["done"], "ast": "<JSON 字符串>"} 或 {"status": ["done", "error"], "problems": ["..."]}
```
`repl/server.go:501-509` -- 返回当前 score 的语法树 JSON。等价于 `alda parse -o ast`。

客户端解析: `json.ParseJSON([]byte(res["ast"].(string)))` (`repl/client.go:854-871`)。

**LLM 可用**: 完整 AST 结构, 可用于分析代码结构。

#### score-text

```
请求: {"op": "score-text"}
响应: {"status": ["done"], "text": "<Alda 代码文本>"}
```
`repl/server.go:511-513` -- 直接返回 `server.input`, 即当前 score 的 Alda 源代码文本。等价于 `alda parse` 的逆向操作。

客户端解析: `res["text"].(string)` (`repl/client.go:797-814`)。

**LLM 可用**: 纯文本, 可检查代码内容。

#### stop

```
请求: {"op": "stop"}
响应: {"status": ["done"]} 或 {"status": ["done", "error"], "problems": ["..."]}
```
`repl/server.go:515-529` -- 向当前绑定的 player 发送 `/system/stop` OSC 消息。

---

## 四、LLM 可用的反馈信号清单

LLM 无法听到音频, 必须通过机器可读符号获取反馈。以下是所有可用的间接反馈通道:

### 4.1 结构化 JSON 信号 (高价值)

| 信号来源 | 调用方式 | 返回内容 | 用途 |
|----------|----------|----------|------|
| parse error | `alda parse -c "..."` (stderr) | `AldaSourceError` 含行列信息 | 语法纠错 |
| score data | `alda parse -o data` 或 nREPL `score-data` | JSON: parts, events count, markers, current-parts | 验证代码是否按预期构建了声部/标记 |
| score events | `alda parse -o events` 或 nREPL `score-events` | JSON 数组: 每个 event 含类型/音符/时值/乐器/音量/声像等 | 精确验证每个音符参数, 最细粒度的程序化反馈 |
| score AST | `alda parse -o ast` 或 nREPL `score-ast` | JSON AST 树 | 验证代码结构 |
| score text | nREPL `score-text` | Alda 源码字符串 | 查询 REPL 服务器当前代码状态 |
| instruments list | `alda instruments` 或 nREPL `instruments` | 乐器名称列表 | 确定可用乐器 |
| process list | `alda ps` | TSV: id/port/state/expiry/type/pid | 监控 player/REPL 进程状态 |
| version | `alda version` 或 nREPL `describe` | 版本字符串 | 环境检查 |
| doctor | `alda doctor` | OK/ERR 逐行状态 | 环境诊断 |

### 4.2 MIDI 二进制分析

**调用方式**: `alda export ...` (CLI) 或 nREPL `export` op

MIDI 文件可通过 MIDI 解析库分析:
- `NoteOn`/`NoteOff` 消息 -- 音符 pitch、velocity、channel
- Control Change 消息 -- program (乐器补丁号)、volume (7)、panning (10)
- 时间戳 -- 音符排列的时间关系

Alda doctor 中就使用了 MIDI 文件验证 (`cmd/doctor.go:409-452`):
```go
// 使用 gitlab.com/gomidi/midi/midireader 解析 MIDI
rdr := midireader.New(bufio.NewReader(midiFile), nil)
// 检查 channel.NoteOn 的 Key() 是否为期望值
switch msg := msg.(type) {
case channel.NoteOn:
    if msg.Key() == expectedNote { ... }
}
```

### 4.3 错误反馈格式

**nREPL 错误响应** (`repl/server.go:91-106`):
```json
{
  "id": "<message-id>",
  "session": "<session-id>",
  "status": ["done", "error"],
  "problems": ["error message 1", "error message 2"]
}
```

错误判断逻辑 (`repl/client.go:698-735`):
1. 检查 `status` 数组是否包含 `"error"`
2. 如包含, 提取 `problems` 数组
3. 每个 problem 是字符串错误描述

**CLI 错误**: 通过 `model.AldaSourceError` 携带源码位置信息, 包装为 `help.UserFacingError` 后输出到 stderr (`cmd/parse.go:135-138`)。

### 4.4 进程状态监控

`alda ps` 输出格式 (`cmd/ps.go:26-43`):
```
id    port    state    expiry    type    pid
abc   27278   ready    10 seconds ago    player    12345
xyz   34223   -        -         repl-server    12346
```

Player 状态生命周期 (从代码推断):
- `starting` -- 正在启动 (`cmd/doctor.go:563-564`)
- `ready` -- 可用但未被使用 (`system/process_management.go:388`)
- `active` -- 正在播放 (`cmd/play.go:261`)
- `finished` -- 播放完成 (`cmd/play.go:339-341`)

---

## 五、人耳专用通道

以下接口 LLM 调用后**只能**获得 success/failure 元信息, **完全无法感知听感质量**:

| 接口 | 调用方式 | LLM 可获得的反馈 |
|------|----------|-----------------|
| **play** | `alda play -c "..."` | stderr: `"Playing..."`; exit code: 0/非0 |
| **play (unpause)** | `alda play` (无参数) | stderr: `"Playing..."`; exit code |
| **eval-and-play** | nREPL `{"op": "eval-and-play", "code": "..."}` | `{"status": ["done"]}` 或 `{"status": ["done", "error"], "problems": [...]}` |
| **replay** | nREPL `{"op": "replay"}` 或 `:play` | 同上 |
| **export** | `alda export ...` 或 nREPL `"export"` | MIDI 二进制 (可程序化分析, 但不是听感) |

**因此 LLM 的典型工作流是**:
1. 生成 Alda 代码
2. 通过 `parse -o events` 或 nREPL `score-events` 验证代码结构和参数
3. 通过 `parse -o data` 或 nREPL `score-data` 验证声部/标记
4. (可选) 通过 `export` 导出 MIDI 并由 MIDI 分析程序验证
5. 通过 `eval-and-play` 让用户 (人) 听
6. 人类反馈语⾔描述 -> LLM 迭代修改

---

## 六、运行环境依赖

### 6.1 二进制依赖

| 组件 | 语言 | 说明 | 文件:行号 |
|------|------|------|-----------|
| `alda` | Go | CLI 客户端, 用户直接调用 | `cmd/root.go` |
| `alda-player` | JVM (Kotlin/Java) | 后台音频播放器进程, 负责 MIDI 合成和音频输出 | `system/process_management.go:464-467` |

**版本匹配**: `alda` 和 `alda-player` 必须是同一版本。`AldaPlayerPath()` 会检查并记录警告 (`system/process_management.go:490-496`)。`alda doctor` 会提示并安装正确版本。

### 6.2 Player 启动与发现机制

**启动**:
- `FillPlayerPool()` 确保至少有 3 个 player 处于 `ready` 或 `starting` 状态 (`system/process_management.go:544-551`)
- 默认在执行几乎所有 alda 命令时自动填充 (`cmd/root.go:307-327`), 例外: `ps`, `shutdown`, `doctor`
- Player 以 `alda-player run` 启动 (`system/process_management.go:508`)
- 可通过 `ALDA_DISABLE_SPAWNING=yes` 环境变量禁止自动启动 (`system/process_management.go:526`)

**发现**:
- 每个 player 进程在 `CachePath("state", "players", <version>, <id>.json)` 写入 JSON 状态文件 (`system/process_management.go:26-28`)
- 状态文件格式 (`system/process_management.go:125-131`):
  ```json
  {"state": "ready", "port": 27278, "expiry": 1234567890, "pid": 12345}
  ```
- `ReadPlayerStates()` 读取所有状态文件 (`system/process_management.go:181-229`)
- `FindAvailablePlayer()` 查找 `state == "ready"` 的 player, 并 ping 确认可达 (`system/process_management.go:381-413`)

**手动指定**: `-p <port>` 或 `-i <player-id>` 参数可绕过自动发现 (`cmd/play.go:235-249`)。

### 6.3 无头环境 / Docker 中的播放注意事项

1. **`--no-audio` flag** (`cmd/doctor.go:64-70`): `alda doctor --no-audio` 跳过第 10 步 (Play score) 和第 11 步 (Export score as MIDI -- 验证部分), 但 MIDI export 本身仍可使用。

2. **`--lazy-audio` flag** (`cmd/doctor.go:318-319`): 在启动 player 时传入 `--lazy-audio`, 延迟 MIDI 系统初始化。`alda doctor` 在无 `--no-audio` 时也自动使用此标志。

3. **Player 状态发现特殊处理** (`cmd/doctor.go:562-564`): 使用 `--no-audio` 时, player 永远不会进入 `ready` 状态 (因为 MIDI 系统未初始化), 所以 discovery 逻辑接受 `state == "starting"`:
   ```go
   if p.State == "ready" || (noAudio && p.State == "starting") {
   ```

4. **`ALDA_DISABLE_SPAWNING=yes`** (`system/process_management.go:526`): 禁止自动填充 player pool。CI 环境使用。

5. **MIDI 合成器/SoundFont**: `alda-player` 内部使用 JVM MIDI 合成器。需要有效的 SoundFont 才能正常发音。`alda doctor` 不直接检查 SoundFont, 但通过实际播放测试间接验证。

6. **Player 日志** (`cmd/doctor.go:470`): 位于 `CachePath("logs", "alda-player.log")`, 使用 XDG 缓存目录:
   - Linux: `~/.cache/alda/logs/alda-player.log`
   - macOS: `~/Library/Caches/alda/logs/alda-player.log`
   - Windows: `%LOCALAPPDATA%\alda\cache\logs\alda-player.log`

7. **OSC 通信**: 全部使用 TCP over `127.0.0.1`, 端口动态分配 (`system/tcp.go:12-27`)。无网络暴露风险, 但也无法远程访问 player。

---

## 附录 A: 关键文件索引

| 文件 | 内容 |
|------|------|
| `doc/alda-repl-server-api.adoc` | nREPL API 文档 |
| `client/repl/server.go` | nREPL 服务器实现, 全部 op handler |
| `client/repl/client.go` | nREPL 客户端实现, `SendMessage()`, `StartSession()` |
| `client/repl/player_management.go` | 服务器端 player 管理循环 |
| `client/repl/validation.go` | nREPL 请求参数校验 |
| `client/cmd/play.go` | `alda play` 子命令 |
| `client/cmd/parse.go` | `alda parse` 子命令 |
| `client/cmd/export.go` | `alda export` 子命令 (MIDI) |
| `client/cmd/repl.go` | `alda repl` 子命令 (含 --message 模式) |
| `client/cmd/stop.go` | `alda stop` 子命令 |
| `client/cmd/instruments.go` | `alda instruments` 子命令 |
| `client/cmd/ps.go` | `alda ps` 子命令 |
| `client/cmd/doctor.go` | `alda doctor` 子命令 (含 --no-audio) |
| `client/cmd/import.go` | `alda import` 子命令 (MusicXML -> Alda) |
| `client/cmd/format.go` | `alda format` 子命令 (实验性) |
| `client/cmd/root.go` | 根命令, player pool 填充, 全局 verbosity |
| `client/cmd/version.go` | `alda version` 子命令 |
| `client/system/process_management.go` | Player 进程管理, 状态文件读写, player 发现 |
| `client/system/dirs.go` | 缓存/配置目录 (XDG) |
| `client/system/tcp.go` | `FindOpenPort()` |
| `client/system/stdin.go` | `ReadStdin()`, `ErrNoInputSupplied` |
| `client/transmitter/osc.go` | OSC 消息构造, 全部 OSC 端点 |
| `client/transmitter/transmitter.go` | `TransmissionContext`, 传输选项 |

---

## 附录 B: REPL 交互式命令 vs nREPL Ops 映射

REPL 交互式命令 `:command` (定义于 `repl/client.go:79-510`) 与 nREPL ops 对应:

| 交互命令 | 对应 nREPL op | 说明 |
|----------|---------------|------|
| `:play` | `replay` | 支持 `from`/`to` 参数 |
| `:stop` | `stop` | 停止播放 |
| `:new` | `new-score` | 重置 score |
| `:load <file>` | `load` | 加载文件内容为新 score |
| `:save <file>` | `score-text` | 获取当前 score 文本后写入文件 |
| `:export <file>` | `export` | 导出 MIDI |
| `:instruments` | `instruments` | 列出乐器 |
| `:score text` | `score-text` | 显示 score 文本 |
| `:score data` | `score-data` | 显示 score 数据 |
| `:score events` | `score-events` | 显示 score 事件 |
| `:score ast` | `score-ast` | 显示 score AST |
| `:version` | `describe` | 显示版本 |
| `:quit` | (本地) | 退出 |
| `:help` | (本地) | 帮助 |

输入任何 Alda 代码 (不以 `:` 开头) 等价于 `eval-and-play` op (`repl/client.go:1146-1147`)。

---

## 附录 C: 完整的 OSC 消息端点

定义于 `transmitter/osc.go`:

| OSC 地址 | 参数 | 用途 | 文件:行号 |
|----------|------|------|-----------|
| `/ping` | (无) | 心跳检测 | `osc.go:19-21` |
| `/system/midi/export` | `filename: string` | 导出 MIDI | `osc.go:23-27` |
| `/system/play` | (无) | 播放/恢复 | `osc.go:29-31` |
| `/system/stop` | (无) | 停止播放 | `osc.go:33-35` |
| `/system/playback-finished` | `offset: int32` | 单次播放完成后通知 | `osc.go:37-41` |
| `/system/shutdown` | `offset: int32` | 关闭 player | `osc.go:43-47` |
| `/system/offset` | `offset: int32` | 设置当前位置 | `osc.go:49-53` |
| `/system/tempo` | `offset: int32, tempo: float32` | 速度变化 | `osc.go:55-60` |
| `/track/{n}/midi/patch` | `channel, offset, patch: int32` | 乐器补丁号 | `osc.go:62-68` |
| `/track/{n}/midi/note` | `channel, offset, note, duration, audibleDuration, velocity: int32` | 音符事件 | `osc.go:70-82` |
| `/track/{n}/midi/volume` | `channel, offset, volume: int32` | 音量 CC | `osc.go:84-89` |
| `/track/{n}/midi/panning` | `channel, offset, panning: int32` | 声像 CC | `osc.go:91-98` |
