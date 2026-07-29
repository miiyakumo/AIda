# Alda 解析与执行管线 (Go Client -> JVM Player)

> **文档生成时间**: 2026-07-27
> **源码基准**: `ref/alda/client/` (Alda 2.4.3, commit `33e17e5674fd98da89462f21b2e0e5f2d9f16944`)
> **重要性**: 每个字段名/类型都标注了 `文件:行号`，harness 解析 JSON 时必须完全对应

---

## 1. 管线概览

```
Alda 源码 (字符串/文件)
  │
  ▼
[Scanner] scanner.go:Scan()           → []Token
  │
  ▼
[Parser]  parser.go:Parse()           → ASTNode (RootNode)
  │
  ├── "alda parse -o ast"             → ASTNode.JSON() 的 JSON 字符串
  │
  ▼
[AST → ScoreUpdate] ast.go:Updates()  → []model.ScoreUpdate
  │
  ├── "alda parse -o events"          → 每个 ScoreUpdate.JSON() 组成的 JSON 数组
  │
  ▼
[Score 构建] score.go:Score.Update()  → *Score (含 Parts, Events (NoteEvent 数组), Markers 等)
  │
  ├── "alda parse -o data"            → score.JSON() 的 JSON 字符串
  │
  ▼
[Transmitter] transmitter/osc.go      → OSC 消息 → alda-player (JVM 进程)
```

关键文件路径：
- `client/parser/scanner.go` - 词法分析
- `client/parser/parser.go` - 语法分析
- `client/parser/ast.go` - AST 节点定义 + AST -> ScoreUpdate 转换
- `client/model/score.go` - Score 数据结构 + 顶级 ScoreUpdate 接口
- `client/model/note.go` - Note, Rest, NoteEvent 类型
- `client/model/chord.go` - Chord
- `client/model/duration.go` - Duration / DurationComponent 类型
- `client/model/pitch.go` - PitchIdentifier, LetterAndAccidentals
- `client/model/attributes.go` - AttributeUpdate, PartUpdate (octave, tempo, volume 等)
- `client/model/voice.go` - VoiceMarker, VoiceGroupEndMarker
- `client/model/barline.go` - Barline
- `client/model/cram.go` - Cram
- `client/model/marker.go` - Marker, AtMarker
- `client/model/repeat.go` - Repeat
- `client/model/repetitions.go` - OnRepetitions
- `client/model/event_sequence.go` - EventSequence
- `client/model/variable.go` - VariableDefinition, VariableReference
- `client/model/lisp.go` - LispList, LispNumber, LispSymbol, LispString 等
- `client/model/part.go` - Part, PartDeclaration
- `client/model/key.go` - KeySignature
- `client/model/midi.go` - MIDI channel 分配逻辑
- `client/model/source_context.go` - AldaSourceContext, AldaSourceError
- `client/transmitter/osc.go` - OSC 发送器
- `client/system/process_management.go` - Player 进程管理
- `client/cmd/parse.go` - `alda parse` 命令实现

---

## 2. Scanner 阶段 (词法分析)

### 2.1 入口

- `scanner.go:773-805` - `Scan(filename, input) ([]Token, error)`
- `scanner.go:808-815` - `ScanFile(filepath) ([]Token, error)`

### 2.2 Token 类型枚举

定义在 `scanner.go:59-97`，共 32 种 token:

```go
Alias, AtMarker, Barline, Colon, CramClose, CramOpen, EOF,
Equals, EventSeqClose, EventSeqOpen, Flat, Integer, LeftParen,
Marker, Name, Natural, NoteLength, NoteLengthMs, NoteLengthSeconds,
NoteLetter, Number, OctaveDown, OctaveSet, OctaveUp, Repetitions,
RestLetter, RightParen, Separator, Sharp, SingleQuote,
String, Symbol, Tie, Repeat, VoiceMarker
```

### 2.3 Token 结构

`scanner.go:100-105`:
```go
type Token struct {
    sourceContext AldaSourceContext  // 含 Filename, Line, Column
    tokenType     TokenType
    text          string
    literal       interface{}        // 多态: rune(NoteLetter), int32, float64, noteLength, []RepetitionRange, string
}
```

### 2.4 Scanner 的错误报告

`scanner.go:21-32` - `errorAtPosition(line, column, msg)` 返回 `*model.AldaSourceError`，包含完整的文件名、行号、列号。

`scanner.go:34-56` - `unexpectedCharError` 生成类似 `"Unexpected 'x' in note/rest/name"` 的错误消息。

---

## 3. AST JSON 结构 (`alda parse -o ast`)

### 3.1 ASTNode 定义

`ast.go:69-74`:
```go
type ASTNode struct {
    Type          ASTNodeType           // 枚举 (共 42 种)
    Literal       interface{}           // 多态值
    Children      []ASTNode
    SourceContext model.AldaSourceContext
}
```

### 3.2 ASTNodeType 枚举 (共 42 种)

`ast.go:14-67`:
```
AtMarkerNode, BarlineNode, ChordNode, CramNode, DenominatorNode,
DotsNode, DurationNode, EventSequenceNode, FirstRepetitionNode,
FlatNode, ImplicitPartNode, LastRepetitionNode, LispListNode,
LispNumberNode, LispQuotedFormNode, LispStringNode, LispSymbolNode,
MarkerNode, NaturalNode, NoteAccidentalsNode, NoteLengthMsNode,
NoteLengthNode, NoteLengthSecondsNode, NoteLetterAndAccidentalsNode,
NoteLetterNode, NoteNode, OctaveDownNode, OctaveSetNode, OctaveUpNode,
OnRepetitionsNode, PartAliasNode, PartDeclarationNode, PartNameNode,
PartNamesNode, PartNode, RepeatNode, RepetitionRangeNode,
RepetitionsNode, RestNode, RootNode, SharpNode, TieNode,
TimesNode, VariableDefinitionNode, VariableNameNode,
VariableReferenceNode, VoiceNode, VoiceGroupEndMarkerNode,
VoiceGroupNode, VoiceNumberNode
```

### 3.3 AST JSON 序列化格式

`ast.go:184-220` - `ASTNode.JSON()`:

每个 AST 节点 JSON 对象包含:
- `"type"`: string -- `node.Type.String()` 的值, 即上面枚举的大驼峰名字符串 (如 `"NoteNode"`, `"ChordNode"`)
- `"children"`: array (可选) -- 子节点的 JSON 数组
- `"literal"`: any (可选) -- 字面值, NoteLetterNode 特殊处理为单字符字符串 (`fmt.Sprintf("%c", literal)`)
- `"source-context"`: object (可选) -- 仅当 `Line > 0` 时包含
  - `"line"`: int
  - `"column"`: int

**示例**: 输入 `"c4"` 的 AST JSON (简化；省略处使用 `...`):
```jsonc
{
  "type": "RootNode",
  "children": [
    {
      "type": "ImplicitPartNode",
      "children": [
        {
          "type": "EventSequenceNode",
          "children": [
            {
              "type": "NoteNode",
              "children": [
                {
                  "type": "NoteLetterAndAccidentalsNode",
                  "children": [
                    {"type": "NoteLetterNode", "literal": "c"},
                    {"type": "NoteAccidentalsNode", ...}
                  ]
                },
                {"type": "DurationNode", "children": [...]}
              ]
            }
          ]
        }
      ]
    }
  ]
}
```

---

## 4. Events JSON 结构 (`alda parse -o events`)

Events 输出是一个 JSON **数组**（见 `cmd/parse.go:187-194`），每个元素是 `model.ScoreUpdate` 接口的 `JSON()` 返回值。

每个 ScoreUpdate JSON 对象都有 `"type"` 字段，区分不同类型：

### 4.1 Note

`note.go:25-37`:
```jsonc
{
  "type": "note",
  "value": {
    "pitch": {
      "letter": "c",
      "accidentals": ["sharp", "flat", ...]
    },
    "duration": { "components": [...] },
    "slurred?": true|false   // 可选, 仅在 Slurred=true 时出现
  }
}
```

- `slurred?` 字段名带问号, `note.go:33`
- `pitch.letter` 是大写字母: A, B, C, D, E, F, G (`pitch.go:33`)
- `pitch.accidentals` 元素是小写字符串: `"flat"`, `"natural"`, `"sharp"` (`pitch.go:93-103`)

### 4.2 Rest

`note.go:223-231`:
```jsonc
{
  "type": "rest",
  "value": {
    "duration": { "components": [...] }  // 可选
  }
}
```

### 4.3 Chord

`chord.go:26-36`:
```jsonc
{
  "type": "chord",
  "value": {
    "events": [ /* ScoreUpdate 数组: Note, Rest, AttributeUpdate, ... */ ]
  }
}
```

### 4.4 Cram

`cram.go:25-38`:
```jsonc
{
  "type": "cram",
  "value": {
    "events": [ /* ScoreUpdate 数组 */ ],
    "duration": { "components": [...] }  // 可选
  }
}
```

### 4.5 EventSequence

`event_sequence.go:21-31`:
```jsonc
{
  "type": "event-sequence",
  "value": {
    "events": [ /* ScoreUpdate 数组 */ ]
  }
}
```

### 4.6 Repeat

`repeat.go:21-29`:
```jsonc
{
  "type": "repeat",
  "value": {
    "event": { /* 被重复的 ScoreUpdate */ },
    "times": 3
  }
}
```

- `times` 类型: int32

### 4.7 OnRepetitions

`repetitions.go:31-46`:
```jsonc
{
  "type": "on-repetitions",
  "value": {
    "repetitions": [
      {"first": 1, "last": 4},
      {"first": 5, "last": 5}
    ],
    "event": { /* ScoreUpdate */ }
  }
}
```

### 4.8 Marker

`marker.go:21-26`:
```json
{
  "type": "marker",
  "value": { "name": "verse1" }
}
```

### 4.9 AtMarker

`marker.go:80-85`:
```json
{
  "type": "at-marker",
  "value": { "name": "verse1" }
}
```

### 4.10 Barline

`barline.go:23-25`:
```json
{ "type": "barline" }
```

没有 `value` 字段。

### 4.11 PartDeclaration

`part.go:26-33`:
```jsonc
{
  "type": "part-declaration",
  "value": {
    "names": ["piano"],
    "alias": "myPiano"  // 可选
  }
}
```

- `names` 是字符串数组, `part.go:27`
- `alias` 仅在非空字符串时存在, `part.go:28-30`

### 4.12 VoiceMarker

`voice.go:78-83`:
```json
{
  "type": "voice-marker",
  "value": { "number": 1 }
}
```

- `number` 类型: int32 (`voice.go:81`)

### 4.13 VoiceGroupEndMarker

`voice.go:126-128`:
```json
{ "type": "voice-group-end-marker" }
```

没有 `value` 字段。

### 4.14 AttributeUpdate

`attributes.go:25-29`:
```json
{
  "type": "attribute-update",
  "attribute": "tempo",
  "value": 120
}
```

- 实际的 JSON 是 `PartUpdate.JSON()` 的结果加上 `"type": "attribute-update"`
- `PartUpdate` 的子类型 (各自序列化为不同 JSON):

| PartUpdate 类型 | JSON 形态 | 来源 |
|---|---|---|
| OctaveSet | `{"attribute":"octave","value":4}` | `attributes.go:288-290` |
| OctaveUp | `{"attribute":"octave","value":"up"}` | `attributes.go:302-304` |
| OctaveDown | `{"attribute":"octave","value":"down"}` | `attributes.go:316-318` |
| TempoSet | `{"attribute":"tempo","value":120.0}` | `attributes.go:236-238` |
| MetricModulation | `{"attribute":"tempo","value":{"ratio":1.5}}` | `attributes.go:261-265` |
| VolumeSet | `{"attribute":"volume","value":0.5421}` | `attributes.go:332-334` |
| TrackVolumeSet | `{"attribute":"track-volume","value":0.787}` | `attributes.go:348-350` |
| PanningSet | `{"attribute":"panning","value":0.5}` | `attributes.go:405-407` |
| QuantizationSet | `{"attribute":"quantization","value":0.9}` | `attributes.go:421-423` |
| DurationSet | `{"attribute":"duration","components":[...]}` | `attributes.go:437-441` |
| KeySignatureSet | `{"attribute":"key-signature","value":{...}}` | `attributes.go:456-459` |
| TranspositionSet | `{"attribute":"transposition","value":2}` | `attributes.go:474-477` |
| ReferencePitchSet | `{"attribute":"reference-pitch","value":440.0}` | `attributes.go:495-498` |
| MidiChannelSet | `{"attribute":"midi-channel","value":3}` | `attributes.go:519-523` |
| DynamicMarking | `{"attribute":"dynamic-marking","value":"mf"}` | `attributes.go:389-391` |

### 4.15 GlobalAttributeUpdate

`attributes.go:182-186`:
```json
{
  "type": "global-attribute-update",
  "attribute": "tempo",
  "value": 120
}
```

- 与 AttributeUpdate 形态相同，但 `"type"` 是 `"global-attribute-update"`

### 4.16 VariableDefinition

`variable.go:46-56`:
```jsonc
{
  "type": "variable-definition",
  "value": {
    "events": [ /* ScoreUpdate 数组 */ ]
  }
}
```

注意: VariableDefinition JSON 的 `value` 中**不包含**变量名！

### 4.17 VariableReference

`variable.go:114-119`:
```json
{
  "type": "variable-reference",
  "value": { "name": "myVar" }
}
```

### 4.18 LispList (S-expression)

`lisp.go:1663-1670`:
```jsonc
{
  "type": "list",
  "value": [ /* LispForm 元素数组 */ ]
}
```

LispForm 子类型:
- LispNumber: `{"type":"number","value":42.0}` (`lisp.go:1522-1524`)
- LispString: `{"type":"string","value":"hello"}` (`lisp.go:1548-1550`)
- LispSymbol: `{"type":"symbol","value":"foo"}` (`lisp.go:1476-1478`)
- LispQuotedForm: `{"type":"quoted-form","value":{...}}` (`lisp.go:1445-1447`)
- LispList: 同上 (递归)

### 4.19 Duration JSON 格式

`duration.go:200-208`:
```json
{
  "components": [
    {"denominator": 4, "dots": 1},
    {"ms": 500},
    {"s": 2.5}
  ]
}
```

DurationComponent 子类型:
- NoteLength: `{"denominator":4.0,"dots":1}` (`duration.go:66-71`), `dots` 是 int32
- NoteLengthMs: `{"ms":500.0}` (`duration.go:133-135`)
- NoteLengthSeconds: `{"s":2.5}` (`duration.go:170-172`)
- Barline: `{"type":"barline"}` (`barline.go:23-25`) -- 注: Barline 同时是 DurationComponent 和 ScoreUpdate

---

## 5. Score JSON 结构 (`alda parse -o data`) -- Harness 最重要的反馈信号

### 5.1 Score 顶级字段

`score.go:66-111` - `Score.JSON()`（结构示意，注释代表被省略的对象）:

```jsonc
{
  "parts": {
    "part001": { /* Part JSON */ },
    "part002": { /* Part JSON */ }
  },
  "current-parts": ["part001"],
  "aliases": {
    "myPiano": ["part001"]
  },
  "events": [ /* NoteEvent JSON 数组 */ ],
  "global-attributes": {
    "0.000000": [ /* PartUpdate JSON 数组 */ ],
    "5000.000000": [ /* PartUpdate JSON 数组 */ ]
  },
  "markers": {
    "verse1": 5000.0
  },
  "variables": {
    "myVar": [ /* ScoreUpdate JSON 数组 */ ]
  }
}
```

字段说明:

| 字段 | 类型 | 说明 | 来源行号 |
|---|---|---|---|
| `"parts"` | object | key=part ID (如 `"part001"`), value=Part JSON | `score.go:67-70` |
| `"current-parts"` | string 数组 | 当前活跃的 part ID 列表 | `score.go:72-75` |
| `"aliases"` | object | key=别名, value=part ID 数组 | `score.go:77-85` |
| `"events"` | NoteEvent 数组 | 排序后的 MIDI 音符事件列表 | `score.go:87-90` |
| `"global-attributes"` | object | key=offset (float 字符串, 10 位有效数字), value=PartUpdate 数组 | `score.go:102-110` |
| `"markers"` | object | key=标记名, value=offset (float64) | `score.go:108` |
| `"variables"` | object | key=变量名, value=ScoreUpdate 数组 | `score.go:92-100` |

**关键**: `events` 字段是 `[]ScoreEvent` 类型的序列化，其中每个元素都是 `NoteEvent`（`score.go:54` 定义 `ScoreEvent` 接口，目前唯一实现是 `NoteEvent`）。这些是**已经过计算**的绝对 MIDI 事件，不再是高层描述。

### 5.2 NoteEvent JSON

`note.go:54-66`:
```json
{
  "part": "part001",
  "midi-channel": 0,
  "midi-note": 60,
  "offset": 0.0,
  "duration": 1000.0,
  "audible-duration": 900.0,
  "volume": 0.5421,
  "track-volume": 0.7874,
  "panning": 0.5
}
```

字段说明:

| 字段 | Go 类型 | 含义 | 来源行号 |
|---|---|---|---|
| `"part"` | string | Part ID (如 `"part001"`) | `note.go:56` |
| `"midi-channel"` | int32 | MIDI 通道 (0-15) | `note.go:57` |
| `"midi-note"` | int32 | MIDI 音符号 (0-127) | `note.go:58` |
| `"offset"` | float64 | 事件相对于乐谱开始的时间偏移 (ms) | `note.go:59` |
| `"duration"` | float64 | 音符总时长 (ms) | `note.go:60` |
| `"audible-duration"` | float64 | 可闻时长 (ms), duration * quantization | `note.go:61` |
| `"volume"` | float64 | 音符力度 [0,1] | `note.go:62` |
| `"track-volume"` | float64 | 轨道音量 [0,1] | `note.go:63` |
| `"panning"` | float64 | 声像 [0,1], 0=左, 0.5=中, 1=右 | `note.go:64` |

注意: NoteEvent **不包含** `"type"` 字段。这是 events 数组里唯一的结构。

### 5.3 Part JSON

`part.go:102-129`:
```json
{
  "id": "part001",
  "name": "piano",
  "stock-instrument": "midi-acoustic-grand-piano",
  "tempo-role": "master",
  "tempo": 120,
  "key-signature": { "B": ["flat"], "E": ["flat"] },
  "transposition": 0,
  "reference-pitch": 440,
  "current-offset": 5000.0,
  "last-offset": 4000.0,
  "octave": 4,
  "volume": 0.5421,
  "track-volume": 0.7874,
  "panning": 0.5,
  "midi-channel": 0,
  "quantization": 0.9,
  "duration": { "components": [{"denominator":4,"dots":0}] },
  "time-scale": 1.0,
  "tempo-values": {
    "0.000000": 120,
    "5000.000000": 140
  }
}
```

- `"stock-instrument"`: string, `part.go:111` -- `part.StockInstrument.Name()`
- `"tempo-role"`: string, 值 "unspecified" 或 "master", `part.go:112`
- `"key-signature"`: object, key=大写字幕(A-G), value=accidental 字符串数组, `part.go:114`
- `"tempo-values"`: object, key=offset(float 字符串), value=tempo(float64), `part.go:127`

### 5.4 global-attributes 结构

`score.go:106` - `"global-attributes"` 是一个对象，key 是 offset 的 float 字符串格式 (如 `"0.000000"`)，value 是 PartUpdate 数组。

`attributes.go:72-84`:
```json
{
  "0.000000": [
    {"attribute": "tempo", "value": 120}
  ],
  "5000.000000": [
    {"attribute": "tempo", "value": {"ratio": 1.5}}
  ]
}
```

---

## 6. 语法错误报告

### 6.1 错误类型

`source_context.go:26-29`:
```go
type AldaSourceError struct {
    Context AldaSourceContext  // Filename, Line, Column
    Err     error
}
```

### 6.2 错误发生的两个阶段

**阶段 1: Scanner (词法分析)**
- `scanner.go:21-32` - `errorAtPosition(line, column, msg)`
- `scanner.go:34-56` - `unexpectedCharError` 报告非法字符
- 错误带文件名: `scanner.go:25` - `Filename: s.filename`
- 错误带行号 + 列号: `scanner.go:26-27` - `Line: line, Column: column`

**阶段 2: Parser (语法分析)**
- `parser.go:99-104` - `errorAtToken(token, msg)` 使用 token 的 sourceContext
- `parser.go:106-123` - `unexpectedTokenError` 报告意外 token
- AST -> ScoreUpdate 转换阶段也会产生错误：`ast.go:370-904` 中 `Updates()` 的错误.

### 6.3 错误消息格式

`source_context.go:50-97` - `AldaSourceError.Error()`:

```
<filename>:<line>:<column> <error message>
```

例如: `test.alda:3:5 Unexpected 'x' at the top level`

### 6.4 错误在 cmd/parse.go 中的处理

`cmd/parse.go:135-141`:
```go
switch err.(type) {
case *model.AldaSourceError:
    err = &help.UserFacingError{Err: err}
}
```

`AldaSourceError` 被包装成 `UserFacingError` 直接打印给用户。这意味着:
- 语法错误**带完整文件名、行号、列号**
- 输出格式: `<file>:<line>:<column> <message>`

**harness 解析错误的方式**:
1. 检查 stderr 输出
2. 正则提取: `^(.+):(\d+):(\d+) (.+)$`
3. 或者是 `exit code != 0` 表示解析失败

---

## 7. 语义错误检测

### 7.1 语义错误发生在 Score 构建阶段 (`alda parse -o data`)

`cmd/parse.go:197-200`:
```go
score := model.NewScore()
err = score.Update(scoreUpdates...)
```

`score.go:129-137` - `Score.Update()`:
```go
func (score *Score) Update(updates ...ScoreUpdate) error {
    for _, update := range updates {
        if err := update.UpdateScore(score); err != nil {
            return &AldaSourceError{Context: update.GetSourceContext(), Err: err}
        }
    }
    return nil
}
```

### 7.2 语义错误类型 (部分列举)

| 错误类别 | 检测位置 | 示例 |
|---|---|---|
| 未定义变量引用 | `variable.go:136` | `undefined variable: foo` |
| 未定义 marker | `marker.go:99` | `Marker undefined: verse2` |
| MIDI 音符超出范围 | `note.go:132` | `MIDI note out of the 0-127 range` |
| MIDI 通道不足 | `midi.go:111-119` | `No MIDI channel available` |
| 不合法 note length | `duration.go:55-60` | `The note length must be a positive number` |
| Part 声明歧义 | `part.go:298-329` | `ambiguous instrument reference` |
| offset 引号无效 | `score.go:195-205` | `is not a valid offset reference` |

**关键**: 语义错误也包裹在 `AldaSourceError` 中（`score.go:133`），带源文件位置。

### 7.3 播放阶段的错误检测

- 播放阶段 (`transmitter/osc.go`) 的 `TransmitScore` 只做格式转换，不做语义检查
- 语义验证**全部在 Score 构建阶段完成**
- 唯一播放阶段的异常是 `"unsupported event"` (`osc.go:419`)，这属于内部 bug，不应由用户触发

---

## 8. Transmitter: Score -> OSC 消息

### 8.1 OSCTransmitter

`transmitter/osc.go:15-17`:
```go
type OSCTransmitter struct {
    Port int
}
```

### 8.2 OSC 消息映射

`osc.go:207-434` - `ScoreToOSCBundle()` 将 Score.Events 转换为 OSC Bundle:

1. 按 `EventOffset()` 排序事件 (`osc.go:257-259`)
2. 添加 tempo 变更消息 (`osc.go:275-279`)
3. 遍历 NoteEvent，为每个发送:
   - MIDI program change: `/track/{N}/midi/patch` (`osc.go:62-68`)
   - MIDI volume CC: `/track/{N}/midi/volume` (`osc.go:84-90`)
   - MIDI panning CC: `/track/{N}/midi/panning` (`osc.go:92-98`)
   - MIDI note on: `/track/{N}/midi/note` (`osc.go:70-82`)
4. 追加 `/system/play` 消息 (`osc.go:423-425`)
5. 如果是 oneOff 模式，追加 `/system/playback-finished` 和 `/system/shutdown` (`osc.go:427-431`)

### 8.3 MIDI Note OSC 消息参数

`osc.go:70-82`:
```
/track/{track}/midi/note {channel} {offset} {note} {duration} {audibleDuration} {velocity}
```

参数类型: 全部 int32。

---

## 9. Player 进程管理

### 9.1 Player 生命周期

`system/process_management.go`:

1. **启动**: `spawnPlayer(playerPath)` (`process_management.go:507-516`) -- 执行 `alda-player run`
2. **确保池**: `FillPlayerPool()` (`process_management.go:522-576`) -- 保持至少 3 个 ready 状态的 player
3. **查找可用 Player**: `FindAvailablePlayer()` (`process_management.go:381-412`) -- 遍历 state 文件找 "ready" 状态的 player
4. **Player 状态文件**: JSON 文件存储在 `CachePath("state", "players", version, id+".json")` (`process_management.go:111-113`)

### 9.2 Player 通信协议

- 传输层: TCP (`osc.go:101` -- `osc.NewClient("127.0.0.1", port, osc.ClientProtocol(osc.TCP))`)
- 应用层: OSC (Open Sound Control)
- 端口: 由 Player 启动时分配，存储在 state 文件中

### 9.3 Player State 结构

`process_management.go:125-131`:
```go
type PlayerState struct {
    State  string `json:"state"`   // "starting", "ready", "playing", "done"
    Port   int    `json:"port"`
    Expiry int64  `json:"expiry"`
    PID    int    `json:"pid"`
    ID     string // 从文件名解析
}
```

---

## 10. 对 Harness 设计的启示

### 10.1 `-o data` 输出的使用

`score.Events` 是 harness 最重要的反馈信号:
- 这是一个**已排序**的 `NoteEvent` 数组
- 每个 NoteEvent 包含: `part`, `midi-channel`, `midi-note`, `offset`, `duration`, `audible-duration`, `volume`, `track-volume`, `panning`
- **没有** `type` 字段，可以直接按索引遍历

### 10.2 `-o events` 命令的使用

- 输出是 `[]ScoreUpdate` 的 JSON 数组
- **每个事件都有 `type` 字段**，需按 `type` 分发解析
- 适用于需要检查**高层语义**的场景（如检查是否解析出正确的 chord/cram 结构）

### 10.3 错误处理

- stderr 输出格式: `<filename>:<line>:<column> <message>`
- exit code != 0 表示失败
- 语义错误和语法错误用同一错误类型 (`AldaSourceError`)，都带位置信息

### 10.4 字段名陷阱

- Note JSON 中的 `"slurred?"` 字段带问号 (`note.go:33`)
- NoteEvent JSON 中 `"audible-duration"` 用连字符 (`note.go:63`)
- Part JSON 中的 `"stock-instrument"`, `"tempo-role"`, `"key-signature"`, `"current-offset"` 都用连字符 (`part.go:108-128`)
- `global-attributes` 的 offset key 格式为 10 位有效数字的字符串如 `"0.000000"` (`attributes.go:80-81`)
- NoteEvent 的 `"midi-channel"` 和 `"midi-note"` 用连字符 (`note.go:57-58`)
- `"type"` 值是特定枚举字符串，不是 Go 源代码中的类型名:
  - 事件: `"note"`, `"rest"`, `"chord"`, `"cram"`, `"barline"`, `"marker"`, `"at-marker"`, `"repeat"`, `"on-repetitions"`, `"event-sequence"`, `"voice-marker"`, `"voice-group-end-marker"`, `"part-declaration"`, `"variable-definition"`, `"variable-reference"`, `"attribute-update"`, `"global-attribute-update"`
  - AST: `"RootNode"`, `"NoteNode"`, `"ChordNode"` 等 (大驼峰 + "Node" 后缀)
  - PartUpdate: `"attribute"` 字段值为 `"octave"`, `"tempo"`, `"volume"` 等
